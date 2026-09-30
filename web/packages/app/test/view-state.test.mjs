import assert from "node:assert/strict";
import test from "node:test";

import { rebuildPreservingView } from "../dist/test/view-state.js";

// Just enough DOM for rebuildPreservingView: a flat list of elements under a
// root, the selectors it uses, focus, selection, and scroll offsets.
function fakeDom() {
  const document = { activeElement: null };
  let elements = [];
  const root = {
    tagName: "MAIN",
    ownerDocument: document,
    contains: (element) => elements.includes(element),
    querySelectorAll: (selector) => elements.filter((element) => matches(element, selector)),
  };
  const make = (tagName, { className = "", attrs = {}, text = "", value, type = "text", readOnly = false } = {}) => {
    const dataset = {};
    for (const [name, attr] of Object.entries(attrs)) {
      if (name.startsWith("data-")) dataset[name.slice(5).replace(/-([a-z])/g, (_, c) => c.toUpperCase())] = attr;
    }
    const element = {
      tagName, className, dataset, attrs, textContent: text, type, readOnly,
      value: value ?? "", defaultValue: value ?? "",
      selectionStart: null, selectionEnd: null, selectionDirection: null,
      scrollTop: 0, scrollLeft: 0,
      getAttribute: (name) => attrs[name] ?? null,
      hasAttribute: (name) => name in attrs,
      focus() { document.activeElement = element; },
      setSelectionRange(start, end, direction) { Object.assign(element, { selectionStart: start, selectionEnd: end, selectionDirection: direction ?? "none" }); },
    };
    return element;
  };
  const replace = (next) => { elements = next; if (!elements.includes(document.activeElement)) document.activeElement = null; };
  return { document, root, make, replace };
}

function matches(element, selector) {
  if (selector === "input, textarea") return element.tagName === "INPUT" || element.tagName === "TEXTAREA";
  if (selector === "[data-scroll-key]") return "data-scroll-key" in element.attrs;
  return element.tagName === selector.toUpperCase();
}

function page(dom, { savedAddress = "", cards = ["1", "2"] } = {}) {
  return [
    dom.make("INPUT", { className: "device-url-input", attrs: { "aria-label": "Public pairing address" }, value: savedAddress, type: "url" }),
    dom.make("INPUT", { className: "pairing-link", attrs: { "aria-label": "One-use device pairing link" }, value: "https://link", readOnly: true }),
    dom.make("DIV", { className: "session-menu-list", attrs: { "data-scroll-key": "sessions" } }),
    ...cards.map((pane) => dom.make("BUTTON", { className: "agent-card", attrs: { "data-view-key": `agent:${pane}` }, text: `Agent ${pane} · ${Math.random()}` })),
  ];
}

test("focus, caret, typed text, and scroll carry over to the rebuilt page", () => {
  const dom = fakeDom();
  dom.replace(page(dom));
  const [address, , list] = dom.root.querySelectorAll("INPUT").concat(dom.root.querySelectorAll("DIV"));
  address.focus();
  address.value = "https://phone.exam";
  address.setSelectionRange(10, 10);
  list.scrollTop = 120;

  rebuildPreservingView(dom.root, () => dom.replace(page(dom)));

  const [rebuilt] = dom.root.querySelectorAll("INPUT");
  assert.notEqual(rebuilt, address, "a new element");
  assert.equal(rebuilt.value, "https://phone.exam");
  assert.equal(dom.document.activeElement, rebuilt);
  assert.deepEqual([rebuilt.selectionStart, rebuilt.selectionEnd], [10, 10]);
  assert.equal(dom.root.querySelectorAll("DIV")[0].scrollTop, 120);
  assert.equal(dom.root.querySelectorAll("INPUT")[1].value, "https://link", "read-only fields are left alone");
});

test("a saved field shows the saved value, but newer typing is never erased", () => {
  const dom = fakeDom();
  dom.replace(page(dom));
  let [address] = dom.root.querySelectorAll("INPUT");
  address.focus();
  address.value = "https://Phone.Example/";

  // The save completed: the page resets the field it submitted, and the
  // rebuilt field shows the address the bridge normalized and stored.
  address.value = address.defaultValue;
  rebuildPreservingView(dom.root, () => dom.replace(page(dom, { savedAddress: "https://phone.example" })));
  [address] = dom.root.querySelectorAll("INPUT");
  assert.equal(address.value, "https://phone.example");
  assert.equal(dom.document.activeElement, address, "focus still carries over");

  // Typing continued while a later save was in flight, so the page did not
  // reset the field: the newer text survives even though the saved value moved.
  address.value = "https://newer.example";
  rebuildPreservingView(dom.root, () => dom.replace(page(dom, { savedAddress: "https://other.example" })));
  [address] = dom.root.querySelectorAll("INPUT");
  assert.equal(address.value, "https://newer.example");
});

test("a focused card keeps focus by its key even when cards reorder and retitle", () => {
  const dom = fakeDom();
  dom.replace(page(dom, { cards: ["1", "2", "3"] }));
  const second = dom.root.querySelectorAll("BUTTON")[1];
  second.focus();

  rebuildPreservingView(dom.root, () => dom.replace(page(dom, { cards: ["3", "2", "1"] })));

  assert.equal(dom.document.activeElement.dataset.viewKey, "agent:2");
});
