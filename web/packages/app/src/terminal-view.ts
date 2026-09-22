import { BridgeError, type BridgeClient, type PaneSnapshot, type StreamHandle, type TerminalFrame } from "@luvus/uhp-client";
import { parseAnsi } from "./ansi.js";
import { button, element } from "./dom.js";
import { uploadTerminalFile } from "./file-upload.js";
import { NativeTerminalInput, type TerminalAction } from "./native-input.js";

export interface TerminalPaneOption {
  pane: PaneSnapshot;
  title: string;
  context: string;
  path: string;
}

export class TerminalView {
  readonly root = element("section", { className: "terminal-screen" });
  #stream: StreamHandle | undefined;
  #streamAttempt = 0;
  #reconnectTimer: ReturnType<typeof setTimeout> | undefined;
  #reconnectRetry = 0;
  #destroyed = false;
  #queuedActions: Array<{
    action: TerminalAction;
    params: Record<string, unknown>;
    resolve: (value: unknown) => void;
    reject: (reason: unknown) => void;
  }> = [];
  #status: HTMLElement | undefined;
  #output = element("div", {
    className: "terminal-output",
    attrs: { role: "log", tabindex: "0", "aria-label": "Terminal output" },
  });
  #paintFrame: number | undefined;
  #pendingPaint: { text: string; cursorOffset: number | undefined; cursorPadding: number } | undefined;
  #input: NativeTerminalInput | undefined;
  #inputHint = element("span", { className: "terminal-input-hint", text: "Tap terminal to type" });
  #attach: HTMLButtonElement | undefined;
  #uploadTail: Promise<void> = Promise.resolve();
  #followTail = true;
  #viewportFrame: number | undefined;
  #viewportChanged = () => this.#syncViewport();
  #paneMenu = element("div", {
    className: "terminal-pane-menu",
    attrs: { role: "menu", "aria-label": "Switch terminal pane", hidden: "" },
  });
  #paneSelector: HTMLButtonElement | undefined;
  #paneSwitcher: HTMLElement | undefined;
  #outsidePaneMenu = (event: PointerEvent) => {
    if (this.#paneMenu.hidden || this.#paneSwitcher?.contains(event.target as Node)) return;
    this.#closePaneMenu();
  };
  #paneMenuKeydown = (event: KeyboardEvent) => {
    if (event.key !== "Escape" || this.#paneMenu.hidden) return;
    event.preventDefault();
    this.#closePaneMenu(true);
  };

  constructor(
    private readonly bridge: BridgeClient,
    private readonly generation: string,
    private readonly pane: PaneSnapshot,
    private readonly control: boolean,
    private readonly canUploadFiles: boolean,
    private readonly streamCursor: boolean,
    private readonly paneOptions: () => TerminalPaneOption[],
    private readonly onSelectPane: (pane: PaneSnapshot) => void,
    onBack: () => void,
  ) {
    const title = pane.agent_name || pane.agent || `Pane ${pane.pane_id}`;
    if (control) {
      this.#input = new NativeTerminalInput(
        (action, params) => this.#action(action, params),
        (message) => this.#inputFailed(message),
        (files) => {
          if (this.canUploadFiles) this.#queueFiles(files);
        },
      );
      this.#input.element.addEventListener("focus", () => {
        this.#inputHint.textContent = "Typing in terminal";
        this.root.classList.add("keyboard-active");
        this.#followTail = true;
        this.#syncViewport(true);
      });
      this.#input.element.addEventListener("blur", () => {
        this.#inputHint.textContent = "Tap terminal to type";
        this.root.classList.remove("keyboard-active");
        this.#syncViewport();
      });
      this.#output.addEventListener("click", () => {
        if (!window.getSelection()?.toString()) this.#input?.focus();
      });
    } else {
      this.#inputHint.textContent = "Read-only terminal";
    }

    const fileInput = element("input", {
      className: "terminal-file-input",
      attrs: { type: "file", multiple: "", "aria-label": "Attach files" },
    }) as HTMLInputElement;
    fileInput.disabled = !canUploadFiles;
    const attach = button("+", "terminal-tool attach-file", () => {
      if (this.canUploadFiles) fileInput.click();
    });
    this.#attach = attach;
    attach.disabled = !canUploadFiles;
    attach.setAttribute("aria-label", "Attach files");
    attach.title = "Attach files";
    fileInput.addEventListener("change", () => {
      const files = Array.from(fileInput.files ?? []);
      fileInput.value = "";
      if (files.length) this.#queueFiles(files);
    });

    if (canUploadFiles) {
      this.root.addEventListener("dragenter", (event) => this.#drag(event));
      this.root.addEventListener("dragover", (event) => this.#drag(event));
      this.root.addEventListener("dragleave", (event) => {
        if (!(event.relatedTarget instanceof Node) || !this.root.contains(event.relatedTarget)) {
          this.root.classList.remove("file-drag-active");
        }
      });
      this.root.addEventListener("drop", (event) => {
        const files = Array.from(event.dataTransfer?.files ?? []);
        if (!files.length) return;
        event.preventDefault();
        this.root.classList.remove("file-drag-active");
        this.#queueFiles(files);
      });
    }

    const keyboard = button("⌨", "terminal-tool keyboard-toggle", () => {
      if (document.activeElement === this.#input?.element) this.#input.blur();
      else this.#input?.focus();
    });
    keyboard.disabled = !control;
    keyboard.setAttribute("aria-label", "Show or hide keyboard");
    keyboard.title = "Keyboard";

    const keySpecs = [
      ["escape", "Esc"],
      ["tab", "Tab"],
      ["up", "↑"],
      ["down", "↓"],
      ["left", "←"],
      ["right", "→"],
      ["ctrl-c", "⌃C"],
    ] as const;
    const keys = keySpecs.map(([key, label]) => {
      const controlButton = button(label, "terminal-tool", () => this.#input?.sendKey(key));
      controlButton.disabled = !control;
      controlButton.setAttribute("aria-label", key === "ctrl-c" ? "Control C" : key);
      controlButton.addEventListener("pointerdown", (event) => event.preventDefault());
      return controlButton;
    });
    const tools = element("div", { className: "terminal-tools" }, attach, keyboard, ...keys);
    const controlsToggle = button("", "terminal-controls-toggle", () => {
      const expanded = this.root.classList.toggle("controls-expanded");
      controlsToggle.setAttribute("aria-expanded", String(expanded));
      controlsToggle.setAttribute("aria-label", expanded ? "Hide terminal controls" : "Show terminal controls");
      controlsToggle.title = expanded ? "Hide terminal controls" : "Show terminal controls";
    });
    controlsToggle.setAttribute("aria-expanded", "false");
    controlsToggle.setAttribute("aria-label", "Show terminal controls");
    controlsToggle.title = "Show terminal controls";
    controlsToggle.addEventListener("pointerdown", (event) => event.preventDefault());

    this.#paneSelector = element("button", {
      className: "terminal-pane-selector",
      attrs: { type: "button", "aria-haspopup": "menu", "aria-expanded": "false" },
      on: { click: () => this.#togglePaneMenu() },
    },
    element("span", { className: "terminal-pane-copy" },
      element("strong", { text: title }),
      element("small", { text: pane.cwd || "Terminal" }),
    ),
    element("span", { className: "terminal-pane-chevron", attrs: { "aria-hidden": "true" } }),
    );
    this.#paneSwitcher = element("div", { className: "terminal-pane-switcher" }, this.#paneSelector, this.#paneMenu);

    this.root.append(
      element("header", { className: "terminal-header" }, headerBackButton(onBack), this.#paneSwitcher),
      this.#output,
      element("div", { className: "terminal-controls-wrap" },
        element("div", { className: "terminal-controls" },
          this.#inputHint,
          tools,
          controlsToggle,
        ),
      ),
      fileInput,
      ...(this.#input ? [this.#input.element] : []),
    );
    this.#output.addEventListener("scroll", () => {
      const distance = this.#output.scrollHeight - this.#output.scrollTop - this.#output.clientHeight;
      this.#followTail = distance < 80;
    }, { passive: true });
    window.visualViewport?.addEventListener("resize", this.#viewportChanged);
    window.visualViewport?.addEventListener("scroll", this.#viewportChanged);
    window.addEventListener("resize", this.#viewportChanged);
    document.addEventListener("pointerdown", this.#outsidePaneMenu);
    document.addEventListener("keydown", this.#paneMenuKeydown);
    this.#syncViewport();
  }

  async start(): Promise<void> {
    if (!this.pane.terminal_id) throw new Error("Pane has no live terminal identity");
    await this.#connectStream(true);
    if (this.control && matchMedia("(pointer: fine)").matches) this.#input?.focus();
  }

  async #connectStream(initial = false): Promise<void> {
    if (this.#destroyed || !this.pane.terminal_id) return;
    if (this.#reconnectTimer) clearTimeout(this.#reconnectTimer);
    this.#reconnectTimer = undefined;
    const attempt = ++this.#streamAttempt;
    const method = this.control ? "terminal.backend.control" : "terminal.backend.observe";
    try {
      await this.bridge.connect();
      const stream = await this.bridge.openStream(method, {
        server_generation: this.generation,
        terminal_id: this.pane.terminal_id,
        pane_id: this.pane.pane_id,
        mode: "recent_unwrapped",
        lines: 120,
        ansi: true,
        ...(this.streamCursor ? { cursor: true } : {}),
      }, (frame) => this.#frame(frame, attempt), (reason) => this.#streamClosed(attempt, reason));
      if (this.#destroyed || attempt !== this.#streamAttempt) {
        stream.close();
        return;
      }
      this.#stream = stream;
      this.#reconnectRetry = 0;
      this.#clearStatus();
      this.#restoreInputHint();
      this.#flushQueuedActions(stream);
    } catch (error) {
      if (this.#destroyed || attempt !== this.#streamAttempt) return;
      this.#stream = undefined;
      if (!recoverableConnectionError(error)) {
        this.#failQueuedActions(error);
        if (initial) throw error;
        this.#showStatus(error instanceof Error ? error.message : "Terminal connection failed");
        return;
      }
      this.#showStatus("Connection interrupted — reconnecting…");
      this.#scheduleReconnect();
    }
  }

  destroy(): void {
    this.#destroyed = true;
    this.#streamAttempt += 1;
    if (this.#reconnectTimer) clearTimeout(this.#reconnectTimer);
    this.#reconnectTimer = undefined;
    this.#failQueuedActions(new BridgeError("Terminal view closed", "closed"));
    this.#input?.destroy();
    this.#stream?.close();
    this.#stream = undefined;
    if (this.#paintFrame !== undefined) cancelAnimationFrame(this.#paintFrame);
    if (this.#viewportFrame !== undefined) cancelAnimationFrame(this.#viewportFrame);
    window.visualViewport?.removeEventListener("resize", this.#viewportChanged);
    window.visualViewport?.removeEventListener("scroll", this.#viewportChanged);
    window.removeEventListener("resize", this.#viewportChanged);
    document.removeEventListener("pointerdown", this.#outsidePaneMenu);
    document.removeEventListener("keydown", this.#paneMenuKeydown);
  }

  #togglePaneMenu(): void {
    if (this.#paneMenu.hidden) this.#openPaneMenu();
    else this.#closePaneMenu(true);
  }

  #openPaneMenu(): void {
    const options = this.paneOptions();
    this.#paneMenu.replaceChildren(...options.map((option) => {
      const active = option.pane.pane_id === this.pane.pane_id
        && option.pane.terminal_id === this.pane.terminal_id;
      return element("button", {
        className: `terminal-pane-option${active ? " active" : ""}`,
        attrs: { type: "button", role: "menuitem", ...(active ? { "aria-current": "true" } : {}) },
        on: { click: () => {
          if (active) {
            this.#closePaneMenu(true);
            return;
          }
          this.#closePaneMenu();
          this.onSelectPane(option.pane);
        } },
      },
      element("span", { className: "terminal-pane-option-dot", attrs: { "aria-hidden": "true" } }),
      element("span", { className: "terminal-pane-option-copy" },
        element("strong", { text: option.title }),
        element("small", { text: option.context }),
        element("small", { className: "terminal-pane-option-path", text: option.path }),
      ),
      );
    }));
    if (options.length === 0) this.#paneMenu.append(element("p", { className: "terminal-pane-menu-empty", text: "No terminal panes available" }));
    this.#paneMenu.hidden = false;
    this.#paneSelector?.setAttribute("aria-expanded", "true");
  }

  #closePaneMenu(restoreFocus = false): void {
    this.#paneMenu.hidden = true;
    this.#paneSelector?.setAttribute("aria-expanded", "false");
    if (restoreFocus) this.#paneSelector?.focus();
  }

  #frame(raw: Record<string, unknown>, attempt: number): void {
    if (this.#destroyed || attempt !== this.#streamAttempt) return;
    if (raw.event === "terminal.resync_required") {
      this.#disconnectStream(attempt, "Terminal changed while the connection was catching up");
      return;
    }
    if (raw.event !== "terminal.frame") return;
    const frame = raw as unknown as TerminalFrame;
    if (typeof frame.data.text === "string") {
      const cursorOffset = frame.data.cursor?.offset;
      const cursorPadding = frame.data.cursor?.padding_cells;
      this.#paint(
        frame.data.text,
        typeof cursorOffset === "number" && Number.isSafeInteger(cursorOffset) && cursorOffset >= 0
          ? cursorOffset
          : undefined,
        typeof cursorPadding === "number" && Number.isSafeInteger(cursorPadding) && cursorPadding >= 0
          ? cursorPadding
          : 0,
      );
    }
  }

  #paint(text: string, cursorOffset?: number, cursorPadding = 0): void {
    this.#pendingPaint = { text, cursorOffset, cursorPadding };
    if (this.#paintFrame !== undefined) return;
    this.#paintFrame = requestAnimationFrame(() => {
      this.#paintFrame = undefined;
      const pending = this.#pendingPaint;
      this.#pendingPaint = undefined;
      if (pending === undefined) return;
      const followTail = this.#followTail;
      const fragment = renderTerminalFrame(pending.text, pending.cursorOffset, pending.cursorPadding);
      this.#output.replaceChildren(fragment);
      if (followTail) this.#scrollToLatest();
    });
  }

  #showStatus(message: string): void {
    if (!this.#status?.isConnected) {
      this.#status = element("div", { className: "terminal-status terminal-connection-status" });
      this.#output.append(this.#status);
    }
    this.#status.textContent = message;
    this.#output.scrollTop = this.#output.scrollHeight;
  }

  #clearStatus(): void {
    this.#status?.remove();
    this.#status = undefined;
  }

  #inputFailed(message: string): void {
    if (!this.#stream) {
      this.#showStatus("Connection interrupted — reconnecting…");
      this.#scheduleReconnect();
      return;
    }
    this.#showStatus(`Input was not delivered: ${message}`);
  }

  async #action(action: TerminalAction, params: Record<string, unknown>): Promise<unknown> {
    this.#followTail = true;
    this.#scrollToLatest();
    const stream = this.#stream;
    if (!stream) {
      if (this.#destroyed) throw new BridgeError("Terminal view is closed", "closed");
      if (this.#queuedActions.length >= 256) {
        throw new BridgeError("Terminal reconnect input buffer is full", "input_buffer_full");
      }
      this.#scheduleReconnect();
      return new Promise((resolve, reject) => {
        this.#queuedActions.push({ action, params, resolve, reject });
      });
    }
    try {
      return await stream.action(action, params);
    } catch (error) {
      if (this.#stream === stream && recoverableConnectionError(error)) {
        this.#disconnectStream(this.#streamAttempt, "Terminal input connection was interrupted");
      }
      throw error;
    }
  }

  #queueFiles(files: File[]): void {
    if (!this.canUploadFiles) return;
    this.#uploadTail = this.#uploadTail.then(async () => {
      if (!files.length) return;
      if (this.#attach) this.#attach.disabled = true;
      this.#inputHint.textContent = `Uploading ${files.length === 1 ? files[0]!.name : `${files.length} files`}`;
      try {
        for (let index = 0; index < files.length; index += 1) {
          await uploadTerminalFile(files[index]!, (action, params) => this.#action(action, params));
          if (index + 1 < files.length) await this.#action("paste_text", { text: " " });
        }
        this.#inputHint.textContent = files.length === 1 ? "File attached" : "Files attached";
        this.#input?.focus();
      } catch (error) {
        const message = error instanceof Error ? error.message : "File upload failed";
        this.#showStatus(`Upload failed: ${message}`);
        this.#inputHint.textContent = "Tap terminal to type";
      } finally {
        if (this.#attach) this.#attach.disabled = !this.canUploadFiles;
      }
    });
  }

  #drag(event: DragEvent): void {
    if (!Array.from(event.dataTransfer?.types ?? []).includes("Files")) return;
    event.preventDefault();
    if (event.dataTransfer) event.dataTransfer.dropEffect = "copy";
    this.root.classList.add("file-drag-active");
  }

  #syncViewport(revealInput = false): void {
    const viewport = window.visualViewport;
    const height = Math.max(1, Math.round(viewport?.height ?? window.innerHeight));
    const top = Math.max(0, Math.round(viewport?.offsetTop ?? 0));
    this.root.style.setProperty("--terminal-viewport-height", `${height}px`);
    this.root.style.setProperty("--terminal-viewport-top", `${top}px`);
    if ((!revealInput && !this.root.classList.contains("keyboard-active")) || !this.#followTail) return;
    this.#scrollToLatest();
  }

  #scrollToLatest(): void {
    this.#output.scrollTop = this.#output.scrollHeight;
    if (this.#viewportFrame !== undefined) cancelAnimationFrame(this.#viewportFrame);
    this.#viewportFrame = requestAnimationFrame(() => {
      this.#viewportFrame = undefined;
      if (this.#followTail) this.#output.scrollTop = this.#output.scrollHeight;
    });
  }

  #streamClosed(attempt: number, reason: string): void {
    if (this.#destroyed || attempt !== this.#streamAttempt) return;
    this.#streamAttempt += 1;
    this.#stream = undefined;
    this.#showStatus(reason ? "Connection interrupted — reconnecting…" : "Reconnecting terminal…");
    this.#scheduleReconnect();
  }

  #disconnectStream(attempt: number, _reason: string): void {
    if (this.#destroyed || attempt !== this.#streamAttempt) return;
    const stream = this.#stream;
    this.#streamAttempt += 1;
    this.#stream = undefined;
    stream?.close();
    this.#showStatus("Connection interrupted — reconnecting…");
    this.#scheduleReconnect();
  }

  #scheduleReconnect(): void {
    if (this.#destroyed || this.#stream || this.#reconnectTimer) return;
    const delay = Math.min(5_000, 150 * 2 ** Math.min(this.#reconnectRetry++, 5));
    this.#reconnectTimer = setTimeout(() => {
      this.#reconnectTimer = undefined;
      void this.#connectStream();
    }, delay);
  }

  #flushQueuedActions(stream: StreamHandle): void {
    const queued = this.#queuedActions;
    this.#queuedActions = [];
    for (const pending of queued) {
      void stream.action(pending.action, pending.params).then(pending.resolve, pending.reject);
    }
  }

  #failQueuedActions(error: unknown): void {
    const queued = this.#queuedActions;
    this.#queuedActions = [];
    for (const pending of queued) pending.reject(error);
  }

  #restoreInputHint(): void {
    if (!this.control) {
      this.#inputHint.textContent = "Read-only terminal";
    } else if (document.activeElement === this.#input?.element) {
      this.#inputHint.textContent = "Typing in terminal";
    } else {
      this.#inputHint.textContent = "Tap terminal to type";
    }
  }

}

function recoverableConnectionError(error: unknown): boolean {
  if (!(error instanceof BridgeError)) return true;
  return new Set(["bridge_error", "closed", "disconnected", "stale_stream", "timeout", "unavailable"]).has(error.code);
}

function headerBackButton(onBack: () => void): HTMLButtonElement {
  const back = button("", "header-back", onBack);
  back.setAttribute("aria-label", "Back");
  back.title = "Back";
  return back;
}

function renderTerminalFrame(text: string, cursorOffset: number | undefined, cursorPadding: number): DocumentFragment {
  const fragment = document.createDocumentFragment();
  let remaining = cursorOffset;
  let placed = false;
  for (const run of parseAnsi(text)) {
    const characters = Array.from(run.text);
    if (!placed && remaining !== undefined && remaining <= characters.length) {
      appendStyledText(fragment, characters.slice(0, remaining).join(""), run.style);
      if (cursorPadding > 0) fragment.append(document.createTextNode(" ".repeat(cursorPadding)));
      fragment.append(element("span", { className: "terminal-caret", attrs: { "aria-hidden": "true" } }));
      appendStyledText(fragment, characters.slice(remaining).join(""), run.style);
      placed = true;
      continue;
    }
    appendStyledText(fragment, run.text, run.style);
    if (!placed && remaining !== undefined) remaining -= characters.length;
  }
  if (!placed && remaining === 0) {
    if (cursorPadding > 0) fragment.append(document.createTextNode(" ".repeat(cursorPadding)));
    fragment.append(element("span", { className: "terminal-caret", attrs: { "aria-hidden": "true" } }));
  }
  return fragment;
}

function appendStyledText(
  fragment: DocumentFragment,
  text: string,
  style: Partial<CSSStyleDeclaration> | undefined,
): void {
  if (!text) return;
  if (!style) {
    fragment.append(document.createTextNode(text));
    return;
  }
  const span = document.createElement("span");
  span.textContent = text;
  Object.assign(span.style, style);
  fragment.append(span);
}
