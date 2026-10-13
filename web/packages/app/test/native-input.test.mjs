import assert from "node:assert/strict";
import test from "node:test";
import { NativeTerminalInput } from "../dist/test/native-input.js";

const guard = "\u200b";
const event = (type, fields = {}, cancelable = true) => Object.assign(new Event(type, { cancelable }), fields);

async function withInput(run, coarse = true) {
  const names = ["document", "window", "requestAnimationFrame", "cancelAnimationFrame"];
  const saved = new Map(names.map((name) => [name, globalThis[name]]));
  const frames = new Map(), actions = [], errors = [], files = [], modifiers = [];
  let nextFrame = 0;
  class Textarea extends EventTarget {
    value = "";
    selectionStart = 0;
    selectionEnd = 0;
    attrs = new Map();
    setAttribute(name, value) { this.attrs.set(name, value); }
    setSelectionRange(start, end) { this.selectionStart = start; this.selectionEnd = end; }
    focus() { document.activeElement = this; }
    blur() { document.activeElement = null; this.dispatchEvent(new Event("blur")); }
    remove() {}
  }
  globalThis.document = { createElement: () => new Textarea(), activeElement: null };
  globalThis.window = { matchMedia: () => ({ matches: coarse }), getSelection: () => ({ toString: () => "" }) };
  globalThis.requestAnimationFrame = (callback) => { frames.set(++nextFrame, callback); return nextFrame; };
  globalThis.cancelAnimationFrame = (id) => frames.delete(id);
  const input = new NativeTerminalInput(async (action, params) => actions.push({ action, params }), (message) => errors.push(message), (value) => files.push(value), (value) => modifiers.push(value));
  const flush = () => { for (const [id, callback] of frames) { frames.delete(id); callback(); } };
  const edit = (inputType, data = null, cancelable = true) => {
    const before = event("beforeinput", { inputType, data, isComposing: false }, cancelable);
    input.element.dispatchEvent(before);
    if (!before.defaultPrevented) {
      const { value, selectionStart: start, selectionEnd: end } = input.element;
      if (inputType.startsWith("delete")) {
        const forward = inputType.endsWith("Forward");
        input.element.value = forward ? value.slice(0, start) + value.slice(Math.max(end, start + 1))
          : value.slice(0, Math.max(0, start - 1)) + value.slice(end);
      } else input.element.value = value.slice(0, start) + (data ?? "") + value.slice(end);
      input.element.dispatchEvent(event("input", { inputType, data, isComposing: false }, false));
    }
    return before;
  };
  const key = (value, modifiers = {}) => {
    const down = event("keydown", { key: value, ctrlKey: false, altKey: false, metaKey: false, shiftKey: false, isComposing: false, ...modifiers });
    input.element.dispatchEvent(down);
    return down;
  };
  try { await run({ input, actions, errors, files, modifiers, edit, key, flush }); }
  finally {
    input.destroy();
    for (const [name, value] of saved) { if (value === undefined) delete globalThis[name]; else globalThis[name] = value; }
  }
}

test("held mobile Backspace keeps native editing alive and forwards once per repeat", () => withInput(({ input, actions, edit, key }) => {
  for (let repeat = 0; repeat < 8; repeat += 1) {
    assert.equal(key("Backspace", { repeat: repeat > 0 }).defaultPrevented, false);
    assert.equal(edit("deleteContentBackward").defaultPrevented, false);
    assert.equal(input.element.value, guard + guard);
    assert.equal(input.element.selectionStart, 1);
  }
  assert.deepEqual(actions, Array.from({ length: 8 }, () => ({ action: "send_key", params: { key: "backspace" } })));
}));

test("input-only and noncancelable mobile deletion intents are forwarded once", () => withInput(({ input, actions, edit }) => {
  for (const inputType of ["deleteContentForward", "deleteWordBackward", "deleteWordForward"]) edit(inputType, null, false);
  input.element.value = guard;
  input.element.dispatchEvent(event("input", { inputType: "deleteContentBackward", data: null }, false));
  assert.deepEqual(actions.map(({ params }) => params.key), ["delete", "ctrl-w", "alt-d", "backspace"]);
  assert.equal(input.element.value, guard + guard);
}));

test("desktop repeated keys and deletion modifiers keep their existing mappings", () => withInput(({ actions, key }) => {
  for (const modifiers of [{}, { repeat: true }, { ctrlKey: true }, { altKey: true }, { metaKey: true }]) {
    assert.equal(key("Backspace", modifiers).defaultPrevented, true);
  }
  assert.deepEqual(actions.map(({ params }) => params.key), ["backspace", "backspace", "ctrl-w", "ctrl-w", "ctrl-u"]);
}, false));

test("existing physical Ctrl shortcuts survive mobile toolbar support", () => withInput(({ actions, key }) => {
  const letters = ["a", "b", "c", "d", "e", "f", "g", "k", "l", "n", "o", "p", "r", "t", "u", "w", "y"];
  for (const letter of letters) assert.equal(key(letter, { ctrlKey: true }).defaultPrevented, true);
  assert.deepEqual(actions.map(({ params }) => params.key), letters.map((letter) => `ctrl-${letter}`));
  assert.equal(key("v", { ctrlKey: true }).defaultPrevented, false, "browser paste remains native");
}));

test("typing stays ordered before deletion without leaking browser guards", () => withInput(({ actions, edit, flush }) => {
  edit("insertText", "hé🙂");
  edit("deleteContentBackward");
  flush();
  assert.deepEqual(actions, [
    { action: "type_literal", params: { text: "hé🙂" } },
    { action: "send_key", params: { key: "backspace" } },
  ]);
}));

test("noncancelable and input-only insertion are sent exactly once", () => withInput(({ input, actions, edit, flush }) => {
  edit("insertText", "abc", false);
  input.element.value = `${guard}日本語${guard}`;
  input.element.dispatchEvent(event("input", { inputType: "insertText", data: null }, false));
  flush();
  assert.deepEqual(actions, [{ action: "type_literal", params: { text: "abc日本語" } }]);
}));

test("native paste and beforeinput/input fallbacks preserve paste semantics and Unicode", () => withInput(({ input, actions, edit }) => {
  const text = `${guard}line one\nline two${guard}`;
  const paste = event("paste", { clipboardData: { files: [], items: [], getData: () => text } });
  input.element.dispatchEvent(paste);
  assert.equal(paste.defaultPrevented, true);
  edit("insertFromPaste", text);
  edit("insertFromPaste", text, false);
  assert.deepEqual(actions, Array.from({ length: 3 }, () => ({ action: "paste_text", params: { text } })));
  assert.equal(input.element.value, guard + guard);
}));

test("composition commits once and does not forward candidate editing", () => withInput(({ input, actions, edit, key, flush }) => {
  input.element.dispatchEvent(event("compositionstart"));
  input.element.value = `${guard}你好${guard}`;
  input.element.dispatchEvent(event("input", { inputType: "insertCompositionText", data: "你好", isComposing: true }, false));
  key("Backspace", { isComposing: true });
  assert.deepEqual(actions, []);
  input.element.dispatchEvent(event("compositionend", { data: "你好" }));
  edit("insertText", "你好"); // Engines may send a final committed input too.
  flush();
  assert.deepEqual(actions, [{ action: "type_literal", params: { text: "你好" } }]);
  assert.equal(input.element.value, guard + guard);
}));

test("file paste still uses uploads and oversized text remains bounded", () => withInput(({ input, actions, files, errors, edit }) => {
  const image = { name: "image.png" };
  input.element.dispatchEvent(event("paste", { clipboardData: { files: [image], items: [], getData: () => "" } }));
  edit("insertFromPaste", "x".repeat(256 * 1024 + 1));
  assert.deepEqual(files, [[image]]);
  assert.deepEqual(actions, []);
  assert.match(errors[0], /256 KiB/);
}));

test("closing an input drops queued typing and sends no later keys", () => withInput(({ input, actions, edit, flush }) => {
  edit("insertText", "not sent");
  input.destroy();
  flush();
  input.sendKey("backspace");
  assert.deepEqual(actions, []);
}));

test("Ctrl is one-shot, keeps typing order, and accepts phone input without keydown", () => withInput(({ input, actions, modifiers, edit, flush }) => {
  edit("insertText", "before");
  input.toggleModifier("ctrl");
  edit("insertText", "a");
  edit("insertText", "after");
  flush();
  assert.deepEqual(actions, [
    { action: "type_literal", params: { text: "before" } },
    { action: "send_key", params: { key: "ctrl-a" } },
    { action: "type_literal", params: { text: "after" } },
  ]);
  assert.deepEqual(modifiers, [{ ctrl: true, shift: false }, { ctrl: false, shift: false }]);
}));

test("Shift changes one typed character and modified Enter and Tab keep their identities", () => withInput(({ input, actions, edit, flush }) => {
  input.toggleModifier("shift");
  edit("insertText", "a");
  edit("insertText", "b");
  input.toggleModifier("shift");
  edit("insertLineBreak");
  input.toggleModifier("shift");
  input.sendKey("tab");
  input.toggleModifier("shift");
  edit("insertText", "1");
  flush();
  assert.deepEqual(actions, [
    { action: "type_literal", params: { text: "Ab" } },
    { action: "send_key", params: { key: "shift-enter" } },
    { action: "send_key", params: { key: "backtab" } },
    { action: "type_literal", params: { text: "!" } },
  ]);
}));

test("combined modifiers work for arrows and hardware Enter without sticking", () => withInput(({ input, actions, modifiers, key }) => {
  input.toggleModifier("ctrl"); input.toggleModifier("shift");
  input.sendKey("left");
  input.sendKey("right");
  input.toggleModifier("ctrl"); input.toggleModifier("shift");
  assert.equal(key("Enter").defaultPrevented, true);
  input.toggleModifier("ctrl");
  assert.equal(key("a").defaultPrevented, true);
  assert.deepEqual(actions.map(({ params }) => params.key), ["ctrl-shift-left", "right", "ctrl-shift-enter", "ctrl-a"]);
  assert.deepEqual(modifiers.at(-1), { ctrl: false, shift: false });
}));

test("Ctrl plus held phone Backspace deletes one word, then repeats ordinary Backspace", () => withInput(({ input, actions, edit, key }) => {
  input.toggleModifier("ctrl");
  for (let repeat = 0; repeat < 3; repeat += 1) {
    assert.equal(key("Backspace", { repeat: repeat > 0 }).defaultPrevented, false);
    edit("deleteContentBackward");
  }
  assert.deepEqual(actions.map(({ params }) => params.key), ["ctrl-w", "backspace", "backspace"]);
}));

test("native clipboard shortcuts and paste clear modifiers without changing clipboard content", () => withInput(({ input, actions, modifiers, edit, key, flush }) => {
  input.toggleModifier("ctrl"); input.toggleModifier("shift");
  edit("insertFromPaste", "unchanged\n日本語");
  input.toggleModifier("shift");
  assert.equal(key("v", { ctrlKey: true }).defaultPrevented, false);
  edit("insertText", "v");
  input.toggleModifier("ctrl");
  assert.equal(key("c", { metaKey: true }).defaultPrevented, false);
  flush();
  assert.deepEqual(actions, [{ action: "paste_text", params: { text: "unchanged\n日本語" } }, { action: "type_literal", params: { text: "v" } }]);
  assert.deepEqual(modifiers.at(-1), { ctrl: false, shift: false });
}));

test("IME, blur and cancellation clear modifiers; invalid Ctrl text is not typed accidentally", () => withInput(({ input, actions, errors, modifiers, edit, flush }) => {
  input.toggleModifier("ctrl"); input.toggleModifier("shift");
  input.element.dispatchEvent(event("compositionstart"));
  input.element.dispatchEvent(event("compositionend", { data: "你好" }));
  input.toggleModifier("shift"); input.blur();
  edit("insertText", "plain");
  input.toggleModifier("ctrl"); edit("insertText", "several letters");
  input.toggleModifier("ctrl"); input.toggleModifier("ctrl");
  edit("insertText", "normal");
  flush();
  assert.equal(errors.length, 1);
  assert.deepEqual(actions, [{ action: "type_literal", params: { text: "你好" } }, { action: "type_literal", params: { text: "plainnormal" } }]);
  assert.deepEqual(modifiers.at(-1), { ctrl: false, shift: false });
}));

test("phone keyboards that compose ASCII still deliver the armed Ctrl chord once", () => withInput(({ input, actions, modifiers, edit, key, flush }) => {
  input.toggleModifier("ctrl"); input.toggleModifier("shift");
  input.element.dispatchEvent(event("compositionstart"));
  edit("insertCompositionText", "a");
  key("Backspace", { isComposing: true });
  assert.deepEqual(actions, []);
  input.element.dispatchEvent(event("compositionend", { data: "a" }));
  edit("insertText", "a");
  flush();
  assert.deepEqual(actions, [{ action: "send_key", params: { key: "ctrl-shift-a" } }]);
  assert.deepEqual(modifiers.at(-1), { ctrl: false, shift: false });
}));

test("selected-text Ctrl+C and AltGr do not become toolbar control chords", () => withInput(({ input, actions, modifiers, edit, key, flush }) => {
  window.getSelection = () => ({ toString: () => "selected output" });
  input.toggleModifier("shift");
  assert.equal(key("c", { ctrlKey: true }).defaultPrevented, false);
  input.toggleModifier("ctrl");
  assert.equal(key("€", { ctrlKey: true, altKey: true }).defaultPrevented, false);
  edit("insertText", "€");
  flush();
  assert.deepEqual(actions, [{ action: "type_literal", params: { text: "€" } }]);
  assert.deepEqual(modifiers.at(-1), { ctrl: false, shift: false });
}));
