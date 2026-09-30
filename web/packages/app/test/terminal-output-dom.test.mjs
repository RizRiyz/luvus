import assert from "node:assert/strict";
import test from "node:test";
import { terminalFrameParts, updateTerminalFrame } from "../dist/test/terminal-output.js";

// A small DOM/Selection fixture for the renderer's node and range operations.
// The same regression is also exercised against a real browser locally.
function withDom(run) {
  const saved = Object.fromEntries(["document", "HTMLElement", "Text", "NodeFilter"].map(key => [key, globalThis[key]]));
  const textNodes = node => node instanceof TextNode ? [node] : node.children.flatMap(textNodes);
  const selection = {
    anchorNode: null, focusNode: null, anchorOffset: 0, focusOffset: 0,
    get isCollapsed() { return this.anchorNode === this.focusNode && this.anchorOffset === this.focusOffset; },
    setBaseAndExtent(anchorNode, anchorOffset, focusNode, focusOffset) { Object.assign(this, { anchorNode, anchorOffset, focusNode, focusOffset }); },
    removeAllRanges() { this.setBaseAndExtent(null, 0, null, 0); },
  };
  class DomNode {
    parentNode = null;
    get nextSibling() { return this.parentNode?.children[this.parentNode.children.indexOf(this) + 1] ?? null; }
  }
  class TextNode extends DomNode {
    constructor(data) { super(); this.data = data; }
    get textContent() { return this.data; }
    get length() { return this.data.length; }
    replaceData(offset, count, text) {
      this.data = this.data.slice(0, offset) + text + this.data.slice(offset + count);
      for (const edge of ["anchor", "focus"]) if (selection[`${edge}Node`] === this) {
        const value = selection[`${edge}Offset`];
        if (value > offset + count) selection[`${edge}Offset`] += text.length - count;
        else if (value > offset) selection[`${edge}Offset`] = offset;
      }
    }
  }
  class ElementNode extends DomNode {
    children = []; className = ""; dataset = {}; style = {};
    get ownerDocument() { return document; }
    get firstChild() { return this.children[0] ?? null; }
    get textContent() { return this.children.map(node => node.textContent).join(""); }
    append(node) { node.parentNode = this; this.children.push(node); }
    contains(node) { return node === this || this.children.some(child => child === node || child instanceof ElementNode && child.contains(node)); }
    setAttribute() {}
    replaceChild(next, old) {
      const index = this.children.indexOf(old);
      this.removeChild(old); next.parentNode = this; this.children.splice(index, 0, next);
    }
    removeChild(node) {
      if (node === selection.anchorNode || node === selection.focusNode
        || node instanceof ElementNode && (node.contains(selection.anchorNode) || node.contains(selection.focusNode))) selection.removeAllRanges();
      this.children.splice(this.children.indexOf(node), 1); node.parentNode = null;
    }
  }
  const offsetOf = (root, target, offset) => {
    let total = 0;
    for (const node of textNodes(root)) { if (node === target) return total + offset; total += node.length; }
    throw new Error("Text point is outside the fixture");
  };
  const document = {
    getSelection: () => selection,
    createElement: () => new ElementNode(),
    createTextNode: text => new TextNode(text),
    createRange() {
      let root, endNode, endOffset;
      return {
        selectNodeContents(node) { root = node; },
        setEnd(node, offset) { endNode = node; endOffset = offset; },
        toString() { return root.textContent.slice(0, offsetOf(root, endNode, endOffset)); },
      };
    },
    createTreeWalker(root) {
      const nodes = textNodes(root); let index = -1;
      return { currentNode: null, nextNode() { this.currentNode = nodes[++index]; return this.currentNode ?? null; } };
    },
  };
  const content = new ElementNode();
  const selectedText = () => {
    if (!selection.anchorNode || !selection.focusNode) return "";
    const anchor = offsetOf(content, selection.anchorNode, selection.anchorOffset);
    const focus = offsetOf(content, selection.focusNode, selection.focusOffset);
    return content.textContent.slice(Math.min(anchor, focus), Math.max(anchor, focus));
  };
  Object.assign(globalThis, { document, HTMLElement: ElementNode, Text: TextNode, NodeFilter: { SHOW_TEXT: 4 } });
  try { run({ content, selection, selectedText }); }
  finally { for (const [key, value] of Object.entries(saved)) { if (value === undefined) delete globalThis[key]; else globalThis[key] = value; } }
}

test("a selected styled run survives removal of a leading ANSI run", () => withDom(({ content, selection, selectedText }) => {
  updateTerminalFrame(content, terminalFrameParts("\x1b[31mremoved\n\x1b[32mretained styled text\x1b[0m\ntail"));
  const selected = content.children[1].firstChild;
  selection.setBaseAndExtent(selected, 0, selected, selected.length);
  updateTerminalFrame(content, terminalFrameParts("\x1b[32mretained styled text\x1b[0m\nnew tail"));
  assert.equal(selectedText(), "retained styled text");
  assert.ok(content.contains(selection.anchorNode) && content.contains(selection.focusNode));
}));

test("backward selections keep their direction after leading text is trimmed", () => withDom(({ content, selection, selectedText }) => {
  updateTerminalFrame(content, terminalFrameParts("\x1b[32mold retained\x1b[0m"));
  const selected = content.firstChild.firstChild;
  selection.setBaseAndExtent(selected, selected.length, selected, 4);
  updateTerminalFrame(content, terminalFrameParts("\x1b[32mretained\x1b[0m"));
  assert.equal(selectedText(), "retained");
  assert.equal(selection.anchorOffset, 8);
  assert.equal(selection.focusOffset, 0);
}));

test("output changes do not steal a selection outside the terminal", () => withDom(({ content, selection }) => {
  const outside = document.createTextNode("outside");
  selection.setBaseAndExtent(outside, 0, outside, 7);
  updateTerminalFrame(content, terminalFrameParts("terminal output"));
  assert.equal(selection.anchorNode, outside);
  assert.equal(selection.focusNode, outside);
}));
