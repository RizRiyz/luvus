import { element } from "./dom.js";
import { deletionInputKey, logicalKey } from "./native-keymap.js";

export type TerminalAction =
  | "type_literal"
  | "paste_text"
  | "paste_image"
  | "send_key"
  | "upload_start"
  | "upload_chunk"
  | "upload_finish"
  | "upload_cancel";

type Dispatch = (action: TerminalAction, params: Record<string, unknown>) => Promise<unknown>;

const MAX_INPUT_BYTES = 256 * 1024;
const encoder = new TextEncoder();

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
  #tail = Promise.resolve();

  constructor(
    private readonly dispatch: Dispatch,
    private readonly onError: (message: string) => void,
    private readonly onFiles: (files: File[]) => void,
  ) {
    this.element.addEventListener("keydown", (event) => this.#onKeyDown(event));
    this.element.addEventListener("beforeinput", (event) => this.#onBeforeInput(event));
    this.element.addEventListener("input", () => this.#onInput());
    this.element.addEventListener("compositionstart", () => { this.#composing = true; });
    this.element.addEventListener("compositionend", (event) => this.#onCompositionEnd(event));
    this.element.addEventListener("paste", (event) => this.#onPaste(event));
  }

  focus(): void {
    if (!this.#destroyed) this.element.focus({ preventScroll: true });
  }

  blur(): void {
    this.element.blur();
  }

  sendKey(key: string): void {
    this.#flushText();
    this.#enqueue("send_key", { key });
    this.focus();
  }

  destroy(): void {
    this.#destroyed = true;
    this.#buffer = "";
    if (this.#flushFrame !== undefined) cancelAnimationFrame(this.#flushFrame);
    this.#flushFrame = undefined;
    this.element.remove();
  }

  #onKeyDown(event: KeyboardEvent): void {
    if (event.isComposing || this.#composing) return;
    const key = logicalKey(event);
    if (!key) return;
    if (key === "ctrl-c" && window.getSelection()?.toString()) return;
    event.preventDefault();
    this.sendKey(key);
  }

  #onBeforeInput(event: InputEvent): void {
    if (event.isComposing || this.#composing) return;
    if (event.inputType === "insertLineBreak" || event.inputType === "insertParagraph") {
      event.preventDefault();
      this.sendKey("enter");
      return;
    }
    const deletionKey = deletionInputKey(event.inputType);
    if (deletionKey) {
      event.preventDefault();
      this.sendKey(deletionKey);
      return;
    }
    if (event.data && event.inputType.startsWith("insert")) {
      event.preventDefault();
      this.element.value = "";
      this.#queueText(event.data);
    }
  }

  #onInput(): void {
    if (this.#composing || !this.element.value) return;
    const text = this.element.value;
    this.element.value = "";
    if (text === this.#ignoreCommittedInput) {
      this.#ignoreCommittedInput = "";
      return;
    }
    this.#ignoreCommittedInput = "";
    this.#queueText(text);
  }

  #onCompositionEnd(event: CompositionEvent): void {
    this.#composing = false;
    const text = event.data || this.element.value;
    this.element.value = "";
    if (!text) return;
    this.#ignoreCommittedInput = text;
    queueMicrotask(() => { this.#ignoreCommittedInput = ""; });
    this.#queueText(text);
  }

  #onPaste(event: ClipboardEvent): void {
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
      this.element.value = "";
      this.#flushText();
      this.onFiles(files);
      return;
    }
    const text = event.clipboardData?.getData("text/plain") ?? "";
    if (!text) return;
    event.preventDefault();
    this.element.value = "";
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
    this.#tail = this.#tail
      .then(() => this.dispatch(action, params))
      .then(() => undefined)
      .catch((error: unknown) => {
        if (this.#destroyed) return;
        const message = error instanceof Error ? error.message : "Terminal input failed";
        this.onError(message);
      });
  }
}
