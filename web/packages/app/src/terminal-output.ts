import { parseAnsi, type AnsiRun } from "./ansi.js";
import { element } from "./dom.js";

export type TerminalPart = ({ kind: "text" } & AnsiRun) | { kind: "cursor" };

/** Cursor offsets count Unicode characters, not UTF-16 units or ANSI bytes. */
export function terminalFrameParts(text: string, cursorOffset?: number, cursorPadding = 0): TerminalPart[] {
  const parts: TerminalPart[] = [];
  let remaining = cursorOffset;
  let placed = false;
  const append = (text: string, style?: AnsiRun["style"]) => {
    if (text) parts.push({ kind: "text", text, ...(style ? { style } : {}) });
  };
  for (const run of parseAnsi(text)) {
    const characters = Array.from(run.text);
    if (!placed && remaining !== undefined && remaining <= characters.length) {
      append(characters.slice(0, remaining).join(""), run.style);
      append(" ".repeat(cursorPadding));
      parts.push({ kind: "cursor" });
      append(characters.slice(remaining).join(""), run.style);
      placed = true;
    } else {
      append(run.text, run.style);
      if (!placed && remaining !== undefined) remaining -= characters.length;
    }
  }
  if (!placed && remaining === 0) {
    append(" ".repeat(cursorPadding));
    parts.push({ kind: "cursor" });
  }
  return parts;
}

/** Keep unchanged text nodes (and selections) alive as terminal frames arrive. */
export function updateTerminalFrame(content: HTMLElement, parts: TerminalPart[]): void {
  const document = content.ownerDocument;
  const selection = document.getSelection();
  let selected: { text: string; anchor: number; focus: number } | undefined;
  if (selection && !selection.isCollapsed && selection.anchorNode && selection.focusNode
    && content.contains(selection.anchorNode) && content.contains(selection.focusNode)) {
    const range = document.createRange();
    range.selectNodeContents(content);
    range.setEnd(selection.anchorNode, selection.anchorOffset);
    const anchor = range.toString().length;
    range.setEnd(selection.focusNode, selection.focusOffset);
    selected = { text: content.textContent ?? "", anchor, focus: range.toString().length };
  }
  let current = content.firstChild;
  for (const part of parts) {
    const className = part.kind === "cursor" ? "terminal-caret" : part.style ? "terminal-run" : undefined;
    let node = current;
    const matches = className
      ? node instanceof HTMLElement && node.className === className
      : node instanceof Text;
    if (!matches) {
      node = className ? element("span", { className }) : document.createTextNode("");
      if (part.kind === "cursor") (node as HTMLElement).setAttribute("aria-hidden", "true");
      if (current) content.replaceChild(node, current);
      else content.append(node);
    }
    if (part.kind === "text") {
      let textNode: Text;
      if (node instanceof Text) textNode = node;
      else {
        const span = node as HTMLElement;
        const styleKey = JSON.stringify(part.style);
        if (span.dataset.terminalStyle !== styleKey) {
          span.style.cssText = "";
          Object.assign(span.style, part.style);
          span.dataset.terminalStyle = styleKey;
        }
        if (!(span.firstChild instanceof Text)) span.append(document.createTextNode(""));
        textNode = span.firstChild as Text;
      }
      updateText(textNode, part.text);
    }
    current = node!.nextSibling;
  }
  while (current) {
    const next = current.nextSibling;
    content.removeChild(current);
    current = next;
  }
  if (selected && selection) {
    const backward = selected.anchor > selected.focus;
    const mapped = retainedTerminalSelection(selected.text, content.textContent ?? "",
      Math.min(selected.anchor, selected.focus), Math.max(selected.anchor, selected.focus));
    const start = mapped && terminalTextPoint(content, mapped[0]);
    const end = mapped && terminalTextPoint(content, mapped[1]);
    if (start && end) {
      const anchor = backward ? end : start;
      const focus = backward ? start : end;
      if (selection.anchorNode !== anchor.node || selection.anchorOffset !== anchor.offset
        || selection.focusNode !== focus.node || selection.focusOffset !== focus.offset) {
        selection.setBaseAndExtent(anchor.node, anchor.offset, focus.node, focus.offset);
      }
    } else selection.removeAllRanges();
  }
}

/** Map unchanged selected text, never stale offsets after a capture drops lines. */
export function retainedTerminalSelection(previous: string, text: string, start: number, end: number): [number, number] | undefined {
  if (start < 0 || end > previous.length || start >= end) return;
  let prefix = 0;
  while (prefix < previous.length && prefix < text.length && previous[prefix] === text[prefix]) prefix += 1;
  if (end <= prefix) return [start, end];
  let suffix = 0;
  while (suffix < previous.length - prefix && suffix < text.length - prefix
    && previous[previous.length - suffix - 1] === text[text.length - suffix - 1]) suffix += 1;
  if (start >= previous.length - suffix) return [start + text.length - previous.length, end + text.length - previous.length];
  const selected = previous.slice(start, end);
  const index = text.indexOf(selected);
  if (index < 0) return;
  if (text.indexOf(selected, index + 1) < 0 && previous.indexOf(selected) === start
    && previous.indexOf(selected, start + 1) < 0) return [index, index + selected.length];
  const contextStart = Math.max(0, start - 32);
  const context = previous.slice(contextStart, end + 32);
  const contextIndex = text.indexOf(context);
  if (contextIndex < 0 || text.indexOf(context, contextIndex + 1) >= 0
    || previous.indexOf(context) !== contextStart
    || previous.indexOf(context, contextStart + 1) >= 0) return;
  const mappedStart = contextIndex + start - contextStart;
  return [mappedStart, mappedStart + selected.length];
}

function terminalTextPoint(content: HTMLElement, offset: number): { node: Text; offset: number } | undefined {
  const walker = content.ownerDocument.createTreeWalker(content, NodeFilter.SHOW_TEXT);
  while (walker.nextNode()) {
    const node = walker.currentNode as Text;
    if (offset <= node.length) return { node, offset };
    offset -= node.length;
  }
}

function updateText(node: Text, text: string): void {
  const previous = node.data;
  if (previous === text) return;
  let prefix = 0;
  while (prefix < previous.length && prefix < text.length && previous[prefix] === text[prefix]) prefix += 1;
  let suffix = 0;
  while (suffix < previous.length - prefix && suffix < text.length - prefix
    && previous[previous.length - suffix - 1] === text[text.length - suffix - 1]) suffix += 1;
  node.replaceData(prefix, previous.length - prefix - suffix, text.slice(prefix, text.length - suffix));
}

export interface TerminalScrollAnchor {
  context: string;
  contextOffset: number;
  top: number;
  scrollTop: number;
}

/** Remember the visible text, including when a bounded capture drops old lines. */
export function captureTerminalScroll(viewport: HTMLElement, content: HTMLElement): TerminalScrollAnchor {
  const anchor = { context: "", contextOffset: 0, top: 0, scrollTop: viewport.scrollTop };
  const document = content.ownerDocument;
  const box = viewport.getBoundingClientRect();
  const style = getComputedStyle(viewport);
  const x = box.left + parseFloat(style.paddingLeft) + 1;
  const y = box.top + 2;
  const position = document.caretPositionFromPoint?.(x, y);
  const fallback = position ? undefined : document.caretRangeFromPoint?.(x, y);
  const node = position?.offsetNode ?? fallback?.startContainer;
  const offset = position?.offset ?? fallback?.startOffset;
  if (!(node instanceof Text) || offset === undefined || !content.contains(node)) return anchor;
  const walker = document.createTreeWalker(content, NodeFilter.SHOW_TEXT);
  let preceding = 0;
  while (walker.nextNode() && walker.currentNode !== node) preceding += (walker.currentNode as Text).length;
  const index = preceding + offset;
  const text = content.textContent ?? "";
  const start = Math.max(0, index - 32);
  const range = document.createRange();
  range.setStart(node, offset);
  range.setEnd(node, Math.min(node.length, offset + 1));
  const rect = range.getClientRects()[0];
  if (!rect) return anchor;
  return { context: text.slice(start, index + 32), contextOffset: index - start, top: rect.top, scrollTop: viewport.scrollTop };
}

export function restoreTerminalScroll(viewport: HTMLElement, content: HTMLElement, anchor: TerminalScrollAnchor): void {
  viewport.scrollTop = anchor.scrollTop;
  if (!anchor.context) return;
  const text = content.textContent ?? "";
  const index = text.indexOf(anchor.context);
  // Repeated or edited text is not reliable evidence of the reader's location.
  if (index < 0 || text.indexOf(anchor.context, index + 1) >= 0) return;
  let remaining = index + anchor.contextOffset;
  const document = content.ownerDocument;
  const walker = document.createTreeWalker(content, NodeFilter.SHOW_TEXT);
  while (walker.nextNode()) {
    const node = walker.currentNode as Text;
    if (remaining >= node.length) {
      remaining -= node.length;
      continue;
    }
    const range = document.createRange();
    range.setStart(node, remaining);
    range.setEnd(node, remaining + 1);
    const rect = range.getClientRects()[0];
    if (rect) viewport.scrollTop += rect.top - anchor.top;
    return;
  }
}
