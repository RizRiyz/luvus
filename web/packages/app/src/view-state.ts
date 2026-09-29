/**
 * Rebuild `root` without disturbing the person using it: focus, the caret in a
 * text field, text typed but not yet submitted, and the scroll position of
 * containers marked with `data-scroll-key` all carry over to the new elements.
 *
 * Elements are matched by `data-view-key` when present, otherwise by tag,
 * class, accessible label, and (for buttons) text, then by their order among
 * elements with the same key.
 */
export function rebuildPreservingView(root: HTMLElement, rebuild: () => void): void {
  const active = root.ownerDocument.activeElement;
  const focused = active instanceof HTMLElement && active !== root && root.contains(active)
    ? { tag: active.tagName, ...locate(root, active) }
    : undefined;
  const caret = focused && isTextField(active) ? readCaret(active) : undefined;

  const edits = new Map<string, string>();
  for (const field of root.querySelectorAll<HTMLInputElement | HTMLTextAreaElement>("input, textarea")) {
    if (isTextField(field) && !field.readOnly && field.value !== field.defaultValue) {
      const place = locate(root, field);
      edits.set(`${place.key}#${place.index}`, field.value);
    }
  }
  const scrolls = new Map<string, [number, number]>();
  for (const container of root.querySelectorAll<HTMLElement>("[data-scroll-key]")) {
    if (container.scrollTop || container.scrollLeft) {
      scrolls.set(container.dataset.scrollKey ?? "", [container.scrollTop, container.scrollLeft]);
    }
  }

  rebuild();

  if (edits.size) {
    for (const field of root.querySelectorAll<HTMLInputElement | HTMLTextAreaElement>("input, textarea")) {
      const place = locate(root, field);
      const value = edits.get(`${place.key}#${place.index}`);
      if (value !== undefined && !field.readOnly) field.value = value;
    }
  }
  for (const container of root.querySelectorAll<HTMLElement>("[data-scroll-key]")) {
    const position = scrolls.get(container.dataset.scrollKey ?? "");
    if (position) [container.scrollTop, container.scrollLeft] = position;
  }
  if (focused) {
    const target = [...root.querySelectorAll<HTMLElement>(focused.tag)]
      .filter((candidate) => viewKey(candidate) === focused.key)[focused.index];
    if (target && !target.hasAttribute("disabled")) {
      target.focus({ preventScroll: true });
      if (caret && isTextField(target)) writeCaret(target, caret);
    }
  }
}

function locate(root: HTMLElement, target: HTMLElement): { key: string; index: number } {
  const key = viewKey(target);
  const same = [...root.querySelectorAll<HTMLElement>(target.tagName)].filter((candidate) => viewKey(candidate) === key);
  return { key, index: Math.max(0, same.indexOf(target)) };
}

function viewKey(element: HTMLElement): string {
  const explicit = element.dataset.viewKey;
  if (explicit) return `key:${explicit}`;
  const label = element.getAttribute("aria-label") ?? element.getAttribute("name") ?? "";
  const text = element.tagName === "BUTTON" ? (element.textContent ?? "").trim().slice(0, 80) : "";
  return `${element.tagName}.${element.className}|${label}|${text}`;
}

type TextField = HTMLInputElement | HTMLTextAreaElement;
type Caret = [number | null, number | null, "forward" | "backward" | "none" | null];

function isTextField(element: unknown): element is TextField {
  if (element instanceof HTMLTextAreaElement) return true;
  return element instanceof HTMLInputElement
    && ["text", "url", "search", "tel", "password", ""].includes(element.type);
}

function readCaret(field: TextField): Caret | undefined {
  try {
    return [field.selectionStart, field.selectionEnd, field.selectionDirection];
  } catch {
    return undefined;
  }
}

function writeCaret(field: TextField, [start, end, direction]: Caret): void {
  if (start === null || end === null) return;
  try {
    field.setSelectionRange(start, end, direction ?? undefined);
  } catch {
    // Not every input type supports a selection.
  }
}
