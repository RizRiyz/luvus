export interface BrowserKey {
  key: string;
  ctrlKey: boolean;
  altKey: boolean;
  metaKey: boolean;
  shiftKey: boolean;
}

const CONTROL_LETTERS = new Set(["a", "b", "c", "d", "e", "f", "g", "k", "l", "n", "o", "p", "r", "t", "u", "w", "y"]);

export interface TerminalModifiers {
  ctrl: boolean;
  shift: boolean;
}

/** One-shot toolbar modifiers use logical keys, never browser escape bytes. */
export function modifiedLogicalKey(key: string, { ctrl, shift }: TerminalModifiers): string | undefined {
  if (/^ctrl-(shift-)?([a-z]|space|enter|tab|up|down|left|right|home|end|pageup|pagedown)$/.test(key) || key === "alt-d") return key;
  if (key === "backspace") return ctrl ? "ctrl-w" : key;
  if (key === "delete") return ctrl ? "alt-d" : key;
  if (key === "escape") return key;
  if (key === "tab" || key === "backtab") {
    return ctrl ? `ctrl-${shift || key === "backtab" ? "shift-" : ""}tab` : shift ? "backtab" : key;
  }
  if (["enter", "up", "down", "left", "right", "home", "end", "pageup", "pagedown"].includes(key)) {
    return `${ctrl ? "ctrl-" : ""}${shift ? "shift-" : ""}${key}`;
  }
  if (ctrl && (/^[a-z]$/i.test(key) || key === "space")) return `ctrl-${shift ? "shift-" : ""}${key.toLowerCase()}`;
  return undefined;
}

/** Shift only the next ASCII character; leave IME, Unicode and pasted text native. */
export function shiftedText(text: string): string {
  const first = text[0] ?? "";
  const plain = "`1234567890-=[]\\;',./";
  const shifted = '~!@#$%^&*()_+{}|:"<>?';
  const index = plain.indexOf(first);
  return (index >= 0 ? shifted[index] : /^[a-z]$/.test(first) ? first.toUpperCase() : first) + text.slice(1);
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
    // Ctrl+V stays the browser paste shortcut.
    if (CONTROL_LETTERS.has(control)) return `ctrl-${control}`;
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
