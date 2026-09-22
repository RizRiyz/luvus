export interface BrowserKey {
  key: string;
  ctrlKey: boolean;
  altKey: boolean;
  metaKey: boolean;
  shiftKey: boolean;
}

/** Translate browser keys into the bounded terminal-control vocabulary. */
export function logicalKey(event: BrowserKey): string | undefined {
  if (event.key === "Backspace") {
    if (event.metaKey) return "ctrl-u";
    if (event.altKey || event.ctrlKey) return "ctrl-w";
  }
  if (event.key === "Delete") {
    if (event.metaKey) return "ctrl-k";
    if (event.altKey || event.ctrlKey) return "alt-d";
  }
  if (event.metaKey || event.altKey) return undefined;
  if (event.ctrlKey && !event.shiftKey) {
    const control = event.key.toLowerCase();
    if (["c", "d", "k", "u", "w"].includes(control)) return `ctrl-${control}`;
    return undefined;
  }
  if (event.ctrlKey) return undefined;
  if (event.key === "Tab") return event.shiftKey ? "backtab" : "tab";
  if (event.shiftKey) return undefined;
  return ({
    Enter: "enter",
    Escape: "escape",
    Backspace: "backspace",
    Delete: "delete",
    ArrowUp: "up",
    ArrowDown: "down",
    ArrowLeft: "left",
    ArrowRight: "right",
    Home: "home",
    End: "end",
    PageUp: "pageup",
    PageDown: "pagedown",
  } as Record<string, string>)[event.key];
}

/** Translate semantic browser deletion intents, including mobile keyboards. */
export function deletionInputKey(inputType: string): string | undefined {
  return ({
    deleteContentBackward: "backspace",
    deleteContentForward: "delete",
    deleteWordBackward: "ctrl-w",
    deleteWordForward: "alt-d",
    deleteSoftLineBackward: "ctrl-u",
    deleteHardLineBackward: "ctrl-u",
    deleteSoftLineForward: "ctrl-k",
    deleteHardLineForward: "ctrl-k",
  } as Record<string, string>)[inputType];
}
