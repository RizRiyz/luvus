import { element } from "./dom.js";
import { deletionInputKey, logicalKey, modifiedLogicalKey, shiftedText, type TerminalModifiers } from "./native-keymap.js";

export type TerminalAction =
  | "type_literal"
  | "paste_text"
  | "set_viewport"
  | "paste_image"
  | "send_key"
  | "upload_start"
  | "upload_chunk"
  | "upload_finish"
  | "upload_cancel";

type Dispatch = (action: TerminalAction, params: Record<string, unknown>) => Promise<unknown>;

const MAX_INPUT_BYTES = 256 * 1024;
const encoder = new TextEncoder();
// Native phone keyboards stop deleting when their editable host is empty.
// Keep one invisible character on each side of the caret; never send these
// browser-only guards to the terminal.
const INPUT_GUARD = "\u200b";

export class NativeTerminalInput {
  readonly element = element("textarea", {
    className: "terminal-input-proxy",
    attrs: {
      "aria-label": "Terminal keyboard input",
      autocomplete: "off",
      autocapitalize: "off",
      autocorrect: "off",
      enterkeyhint: "enter",
      inputmode: "text",
      rows: "1",
      spellcheck: "false",
    },
  }) as HTMLTextAreaElement;

  #buffer = "";
  #composing = false;
  #destroyed = false;
  #flushFrame: number | undefined;
  #ignoreCommittedInput = "";
  #handledBeforeInput: string | undefined;
  #modifiers: TerminalModifiers = { ctrl: false, shift: false };

  constructor(
    private readonly dispatch: Dispatch,
    private readonly onError: (message: string) => void,
    private readonly onFiles: (files: File[]) => void,
    private readonly onModifiersChanged: (modifiers: Readonly<TerminalModifiers>) => void = () => {},
  ) {
    this.#resetInput();
    this.element.addEventListener("keydown", (event) => this.#onKeyDown(event));
    this.element.addEventListener("beforeinput", (event) => this.#onBeforeInput(event));
    this.element.addEventListener("input", (event) => this.#onInput(event as InputEvent));
    this.element.addEventListener("compositionstart", () => {
      this.#composing = true;
      this.element.setAttribute("data-empty", "false");
    });
    this.element.addEventListener("compositionend", (event) => this.#onCompositionEnd(event));
    this.element.addEventListener("paste", (event) => this.#onPaste(event));
    this.element.addEventListener("blur", () => {
      this.clearModifiers();
      this.#flushText();
    });
  }

  focus(): void {
    if (!this.#destroyed) this.element.focus({ preventScroll: true });
  }

  blur(): void {
    this.element.blur();
  }

  sendKey(key: string): void {
    const modifiers = this.#modifiers;
    this.clearModifiers();
    const chord = modifiers.ctrl || modifiers.shift ? modifiedLogicalKey(key, modifiers) : key;
    if (!chord) {
      this.onError("This key does not support the selected terminal modifiers.");
      return;
    }
    this.#sendLogicalKey(chord);
  }

  toggleModifier(modifier: keyof TerminalModifiers): void {
    if (this.#destroyed || this.#composing) return;
    this.focus();
    this.#modifiers = { ...this.#modifiers, [modifier]: !this.#modifiers[modifier] };
    this.onModifiersChanged(this.#modifiers);
  }

  clearModifiers(): void {
    if (!this.#modifiers.ctrl && !this.#modifiers.shift) return;
    this.#modifiers = { ctrl: false, shift: false };
    this.onModifiersChanged(this.#modifiers);
  }

  #sendLogicalKey(key: string): void {
    this.#flushText();
    this.#enqueue("send_key", { key });
    this.focus();
  }

  destroy(): void {
    this.clearModifiers();
    this.#destroyed = true;
    this.#buffer = "";
    if (this.#flushFrame !== undefined) cancelAnimationFrame(this.#flushFrame);
    this.#flushFrame = undefined;
    this.element.remove();
  }

  #onKeyDown(event: KeyboardEvent): void {
    if (event.isComposing || this.#composing) return;
    this.#handledBeforeInput = undefined;
    // Let touch keyboards perform their native edit and emit beforeinput/input.
    // Cancelling Backspace here can stop the OS's held-key repetition.
    if ((event.key === "Backspace" || event.key === "Delete")
      && !event.ctrlKey && !event.altKey && !event.metaKey
      && window.matchMedia("(pointer: coarse)").matches) return;
    // Actual browser copy/paste shortcuts remain native. A deliberately armed
    // toolbar Ctrl is distinct from those physical shortcuts.
    const copy = event.ctrlKey && event.key.toLowerCase() === "c"
      && (event.shiftKey || !!window.getSelection()?.toString());
    if ((event.metaKey && event.key !== "Backspace" && event.key !== "Delete")
      || event.ctrlKey && event.key.toLowerCase() === "v" || copy) {
      this.clearModifiers();
      return;
    }
    if (event.altKey) this.clearModifiers(); // AltGr and Option text remain browser-native.
    if ((this.#modifiers.ctrl || this.#modifiers.shift) && !event.altKey && !event.metaKey) {
      const base = logicalKey({ ...event, key: event.key, ctrlKey: false, altKey: false, metaKey: false, shiftKey: false })
        ?? (event.key === " " ? "space" : event.key);
      const key = modifiedLogicalKey(base, { ctrl: this.#modifiers.ctrl || event.ctrlKey, shift: this.#modifiers.shift || event.shiftKey });
      if (key) {
        event.preventDefault();
        this.clearModifiers();
        this.#sendLogicalKey(key);
      }
      // Printable Shift input arrives through beforeinput; do not synthesize
      // a second character or interfere with the phone keyboard's own edits.
      return;
    }
    const key = logicalKey(event);
    if (!key) return;
    if (key === "ctrl-c" && window.getSelection()?.toString()) return;
    event.preventDefault();
    this.sendKey(key);
  }

  #onBeforeInput(event: InputEvent): void {
    if (event.isComposing || this.#composing) return;
    this.#handledBeforeInput = undefined;
    if (event.inputType === "insertLineBreak" || event.inputType === "insertParagraph") {
      if (!event.cancelable) return;
      event.preventDefault();
      this.#resetInput();
      this.sendKey("enter");
      return;
    }
    const deletionKey = deletionInputKey(event.inputType);
    if (deletionKey) {
      // The native edit must complete so mobile keyboards keep repeating.
      // Its following input event only restores the guards, not another key.
      this.#handledBeforeInput = event.inputType;
      this.sendKey(deletionKey);
      return;
    }
    if (event.cancelable && event.data && event.inputType.startsWith("insert")) {
      event.preventDefault();
      this.#resetInput();
      this.#commitText(event.data, event.inputType === "insertFromPaste");
    }
  }

  #onInput(event: InputEvent): void {
    if (event.isComposing || this.#composing) return;
    const handled = this.#handledBeforeInput !== undefined && this.#handledBeforeInput === event.inputType;
    this.#handledBeforeInput = undefined;
    const text = event.data ?? this.#proxyText();
    this.#resetInput();
    if (handled) return;
    const deletionKey = deletionInputKey(event.inputType);
    if (deletionKey) this.sendKey(deletionKey);
    else if (event.inputType === "insertLineBreak" || event.inputType === "insertParagraph") this.sendKey("enter");
    else this.#commitText(text, event.inputType === "insertFromPaste");
  }

  #resetInput(): void {
    this.#handledBeforeInput = undefined;
    this.element.value = INPUT_GUARD + INPUT_GUARD;
    this.element.setSelectionRange(1, 1);
    this.element.setAttribute("data-empty", "true");
  }

  #proxyText(): string {
    let text = this.element.value;
    if (text.startsWith(INPUT_GUARD)) text = text.slice(1);
    if (text.endsWith(INPUT_GUARD)) text = text.slice(0, -1);
    return text;
  }

  #commitText(text: string, paste = false): void {
    if (!text) return;
    if (text === this.#ignoreCommittedInput) {
      this.#ignoreCommittedInput = "";
      return;
    }
    this.#ignoreCommittedInput = "";
    if (paste) this.#pasteText(text);
    else this.#forwardText(text);
  }

  #forwardText(text: string, composition = false): void {
    const modifiers = this.#modifiers;
    this.clearModifiers();
    if (modifiers.ctrl) {
      const key = /^[a-z ]$/i.test(text) ? modifiedLogicalKey(text === " " ? "space" : text, modifiers) : undefined;
      if (key) this.#sendLogicalKey(key);
      else if (composition) this.#queueText(text);
      else this.onError("Ctrl needs one letter or Space. Paste and IME input are sent unchanged.");
    } else this.#queueText(modifiers.shift ? shiftedText(text) : text);
  }

  #onCompositionEnd(event: CompositionEvent): void {
    this.#composing = false;
    const text = event.data || this.#proxyText();
    this.#resetInput();
    if (!text) {
      this.clearModifiers();
      return;
    }
    this.#ignoreCommittedInput = text;
    queueMicrotask(() => { this.#ignoreCommittedInput = ""; });
    this.#forwardText(text, true);
  }

  #onPaste(event: ClipboardEvent): void {
    this.clearModifiers();
    const transfer = event.clipboardData;
    const files = Array.from(transfer?.files ?? []);
    if (!files.length) {
      for (const item of Array.from(transfer?.items ?? [])) {
        const file = item.kind === "file" ? item.getAsFile() : null;
        if (file) files.push(file);
      }
    }
    if (files.length) {
      event.preventDefault();
      this.#resetInput();
      this.#flushText();
      this.onFiles(files);
      return;
    }
    const text = event.clipboardData?.getData("text/plain") ?? "";
    if (!text) return;
    event.preventDefault();
    this.#resetInput();
    this.#pasteText(text);
  }

  #pasteText(text: string): void {
    this.clearModifiers();
    this.#flushText();
    if (encoder.encode(text).byteLength > MAX_INPUT_BYTES) {
      this.onError("Paste is larger than the 256 KiB terminal input limit.");
      return;
    }
    this.#enqueue("paste_text", { text });
  }

  #queueText(text: string): void {
    this.#buffer += text;
    if (this.#flushFrame !== undefined) return;
    this.#flushFrame = requestAnimationFrame(() => {
      this.#flushFrame = undefined;
      this.#flushText();
    });
  }

  #flushText(): void {
    if (this.#flushFrame !== undefined) cancelAnimationFrame(this.#flushFrame);
    this.#flushFrame = undefined;
    if (!this.#buffer) return;
    const text = this.#buffer;
    this.#buffer = "";
    if (encoder.encode(text).byteLength > MAX_INPUT_BYTES) {
      this.onError("Input is larger than the 256 KiB terminal input limit.");
      return;
    }
    this.#enqueue("type_literal", { text });
  }

  #enqueue(action: TerminalAction, params: Record<string, unknown>): void {
    if (this.#destroyed) return;
    // WebSocket frames and the upstream terminal stream both preserve send
    // order. Do not serialize input on round-trip acknowledgements: doing so
    // makes fast typing accumulate one network RTT per key on remote links.
    void this.dispatch(action, params).catch((error: unknown) => {
      if (this.#destroyed) return;
      const message = error instanceof Error ? error.message : "Terminal input failed";
      this.onError(message);
    });
  }
}
