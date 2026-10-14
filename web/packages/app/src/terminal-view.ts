import { BridgeError, type BridgeClient, type PaneSnapshot, type SessionSnapshot, type StreamHandle, type TerminalFrame } from "@luvus/uhp-client";
import { button, element } from "./dom.js";
import { uploadTerminalFile } from "./file-upload.js";
import { NativeTerminalInput, type TerminalAction } from "./native-input.js";
import { navigationBrand, trapNavigationFocus, type SidebarControls } from "./sidebar-controls.js";
import { terminalWorkspaceOptions, type TerminalPaneOption } from "./terminal-pane-options.js";
import { TerminalPaneSidebar, type TerminalPaneFilter } from "./terminal-pane-sidebar.js";
import { TerminalWorkspaceSidebar } from "./terminal-workspace-sidebar.js";
import { recoverableConnectionError } from "./terminal-reconnect.js";
import { captureTerminalScroll, restoreTerminalScroll, terminalFrameParts, updateTerminalFrame } from "./terminal-output.js";
import { TerminalTargetTracker, type TerminalTarget } from "./terminal-target.js";
import { sameViewport, terminalViewportFor, type TerminalViewport } from "./terminal-viewport.js";

export class TerminalView {
  readonly root = element("section", { className: "terminal-screen" });
  #stream: StreamHandle | undefined;
  #target: TerminalTarget | undefined;
  #targetTracker: TerminalTargetTracker;
  #snapshot: SessionSnapshot;
  #streamAttempt = 0;
  #reconnectTimer: ReturnType<typeof setTimeout> | undefined;
  #reconnectRetry = 0;
  #targetError: BridgeError | undefined;
  #destroyed = false;
  #queuedActions: Array<{
    action: TerminalAction;
    params: Record<string, unknown>;
    resolve: (value: unknown) => void;
    reject: (reason: unknown) => void;
  }> = [];
  #status: HTMLElement | undefined;
  #content = element("div", { className: "terminal-content" });
  #output = element("div", {
    className: "terminal-output",
    attrs: { role: "log", tabindex: "0", "aria-label": "Terminal output" },
  }, this.#content);
  #paintFrame: number | undefined;
  #pendingPaint: { text: string; cursorOffset: number | undefined; cursorPadding: number } | undefined;
  #renderedPaint: { text: string; cursorOffset: number | undefined; cursorPadding: number } | undefined;
  #input: NativeTerminalInput | undefined;
  #inputHint = element("span", { className: "terminal-input-hint", text: "Tap terminal to type" });
  #attach: HTMLButtonElement | undefined;
  #uploadTail: Promise<void> = Promise.resolve();
  #followTail = true;
  #viewportFrame: number | undefined;
  #viewportChanged = () => {
    this.#syncViewport();
    this.#scheduleViewport();
  };
  // One measured cell; the server is asked for the size that fits the output area.
  #cellProbe = element("span", { className: "terminal-cell-probe", attrs: { "aria-hidden": "true" }, text: "0000000000" });
  #lastViewport: TerminalViewport | undefined;
  #viewportTimer: ReturnType<typeof setTimeout> | undefined;
  #outputResize: ResizeObserver | undefined;
  #main: HTMLElement;
  #header: HTMLElement;
  #contentFrame = element("div", { className: "web-content-frame" });
  #navigation = element("div", { className: "terminal-navigation", attrs: { id: "terminal-navigation", "aria-label": "Workspaces and panes" } });
  #navigationOpen = false;
  #paneSelector: HTMLButtonElement | undefined;
  #sidebar: TerminalPaneSidebar;
  #workspaces: TerminalWorkspaceSidebar;
  #desktopNavigation = matchMedia("(min-width: 1024px)");
  #navigationChanged = () => {
    if (this.#desktopNavigation.matches) this.#closeNavigation();
    this.#placePaneSidebar();
    this.#updateSidebar();
    this.#updateWorkspaces();
  };
  #navigationKeydown = (event: KeyboardEvent) => {
    if (!this.#navigationOpen) return;
    if (event.key === "Escape") {
      event.preventDefault();
      this.#closeNavigation(true);
    } else trapNavigationFocus(event, this.#navigation);
  };

  constructor(
    private readonly bridge: BridgeClient,
    snapshot: SessionSnapshot,
    pane: PaneSnapshot,
    private readonly control: boolean,
    private readonly canUploadFiles: boolean,
    private readonly streamCursor: boolean,
    private readonly viewportSizing: boolean,
    private readonly paneOptions: () => TerminalPaneOption[],
    private readonly onSelectPane: (pane: PaneSnapshot) => void,
    onBack: () => void,
    paneFilter: TerminalPaneFilter,
    private readonly sidebarControls: SidebarControls,
  ) {
    this.#snapshot = snapshot;
    this.#targetTracker = new TerminalTargetTracker(snapshot, pane);
    this.#target = this.#targetTracker.resolve(snapshot);
    if (control) {
      this.#input = new NativeTerminalInput(
        (action, params) => this.#action(action, params),
        (message) => this.#inputFailed(message),
        (files) => {
          if (this.canUploadFiles) this.#queueFiles(files);
        },
      );
      this.#input.element.addEventListener("focus", () => this.#scheduleViewport(true));
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
      attrs: { type: "button", "aria-label": "Open navigation", title: "Open navigation", "aria-haspopup": "dialog", "aria-expanded": "false", "aria-controls": "terminal-navigation" },
      on: { click: () => this.#openNavigation() },
    },
    element("span", { className: "terminal-pane-dot", attrs: { "aria-hidden": "true" } }),
    );
    const paneSwitcher = element("div", { className: "terminal-pane-switcher" }, this.#paneSelector);
    const selectPane = (selected: PaneSnapshot) => {
      const wasOpen = this.#navigationOpen;
      this.#closeNavigation(wasOpen);
      if (selected.pane_id === this.#target?.pane.pane_id && selected.terminal_id === this.#target?.pane.terminal_id) {
        if (!wasOpen) this.#input?.focus();
      } else this.onSelectPane(selected);
    };
    this.#sidebar = new TerminalPaneSidebar(selectPane, paneFilter, () => this.#closeNavigation(true), () => sidebarControls.toggle("panes"));
    this.#sidebar.closeButton.setAttribute("aria-label", "Close navigation");
    this.#sidebar.closeButton.setAttribute("aria-controls", "terminal-navigation");
    this.#sidebar.closeButton.title = "Close navigation";
    this.#workspaces = new TerminalWorkspaceSidebar(selectPane, onBack, () => sidebarControls.toggle("workspaces"));
    this.#workspaces.setFooter(sidebarControls.footer());
    this.#navigation.append(
      element("header", { className: "terminal-navigation-header" }, navigationBrand(onBack), this.#sidebar.closeButton),
      this.#workspaces.root,
      this.#sidebar.root,
    );
    this.updateSidebarLayout();
    this.#updateSidebar();
    this.#updateWorkspaces();

    this.#header = element("header", { className: "terminal-header" }, navigationBrand(onBack), paneSwitcher);
    this.#main = element("div", { className: "terminal-main" },
      this.#output,
      element("div", { className: "terminal-controls-wrap" },
        element("div", { className: "terminal-controls" },
          this.#inputHint,
          tools,
          controlsToggle,
        ),
      ),
    );
    this.#contentFrame.append(this.#header, this.#main);
    this.root.append(
      this.#navigation,
      this.#contentFrame,
      fileInput,
      ...(this.#input ? [this.#input.element] : []),
    );
    this.#output.append(this.#cellProbe);
    this.#placePaneSidebar();
    if (this.viewportSizing) {
      this.#outputResize = new ResizeObserver(() => this.#scheduleViewport());
      this.#outputResize.observe(this.#output);
    }
    this.#output.addEventListener("scroll", () => {
      const distance = this.#output.scrollHeight - this.#output.scrollTop - this.#output.clientHeight;
      this.#followTail = distance < 80;
    }, { passive: true });
    window.visualViewport?.addEventListener("resize", this.#viewportChanged);
    window.visualViewport?.addEventListener("scroll", this.#viewportChanged);
    window.addEventListener("resize", this.#viewportChanged);
    document.addEventListener("keydown", this.#navigationKeydown);
    this.#desktopNavigation.addEventListener("change", this.#navigationChanged);
    this.#syncViewport();
  }

  async start(): Promise<void> {
    if (!this.#target) throw new Error("Pane has no live terminal identity");
    await this.#connectStream(true);
    if (this.control && !this.#navigationOpen && matchMedia("(pointer: fine)").matches) this.#input?.focus();
  }

  async #connectStream(initial = false): Promise<void> {
    if (this.#destroyed) return;
    const target = this.#target;
    if (!target?.pane.terminal_id) {
      if (initial) throw new Error("Pane has no live terminal identity");
      return;
    }
    if (this.#reconnectTimer) clearTimeout(this.#reconnectTimer);
    this.#reconnectTimer = undefined;
    const attempt = ++this.#streamAttempt;
    const method = this.control ? "terminal.backend.control" : "terminal.backend.observe";
    try {
      await this.bridge.connect();
      const stream = await this.bridge.openStream(method, {
        server_generation: target.serverGeneration,
        terminal_id: target.pane.terminal_id,
        pane_id: target.pane.pane_id,
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
      this.#lastViewport = undefined;
      this.#scheduleViewport(true);
    } catch (error) {
      if (this.#destroyed || attempt !== this.#streamAttempt) return;
      this.#stream = undefined;
      if (!recoverableConnectionError(error, this.#reconnectRetry)) {
        this.#failQueuedActions(error);
        if (initial) throw error;
        this.#showStatus(error instanceof Error ? error.message : "Terminal connection failed");
        return;
      }
      this.#showStatus(error instanceof BridgeError && error.code === "control_conflict"
        ? "Waiting for terminal control…"
        : "Connection interrupted — reconnecting…");
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
    if (this.#viewportTimer !== undefined) clearTimeout(this.#viewportTimer);
    this.#outputResize?.disconnect();
    window.visualViewport?.removeEventListener("resize", this.#viewportChanged);
    window.visualViewport?.removeEventListener("scroll", this.#viewportChanged);
    window.removeEventListener("resize", this.#viewportChanged);
    document.removeEventListener("keydown", this.#navigationKeydown);
    this.#desktopNavigation.removeEventListener("change", this.#navigationChanged);
  }

  updateSnapshot(snapshot: SessionSnapshot): void {
    if (this.#destroyed) return;
    if (snapshot.session !== this.#targetTracker.session) {
      this.#closeNavigation();
      this.#sidebar.update([], undefined);
      this.#workspaces.update([]);
      this.#snapshot = snapshot;
      this.#target = undefined;
      if (this.#reconnectTimer) clearTimeout(this.#reconnectTimer);
      this.#reconnectTimer = undefined;
      const stream = this.#stream;
      this.#streamAttempt += 1;
      this.#stream = undefined;
      stream?.close();
      this.#targetError = new BridgeError("Terminal belongs to another session", "stale_stream");
      this.#failQueuedActions(this.#targetError);
      this.#showStatus("Session changed — return to Mission Control to choose a terminal");
      return;
    }
    const previous = this.#target;
    this.#snapshot = snapshot;
    const generationChanged = snapshot.server_generation !== this.#targetTracker.serverGeneration;
    const target = this.#targetTracker.resolve(snapshot);
    this.#target = target;
    this.#updateSidebar();
    this.#updateWorkspaces();
    if (!target) {
      if (this.#reconnectTimer) clearTimeout(this.#reconnectTimer);
      this.#reconnectTimer = undefined;
      const stream = this.#stream;
      this.#streamAttempt += 1;
      this.#stream = undefined;
      stream?.close();
      if (generationChanged) {
        this.#targetError = new BridgeError(
          "The server restarted; choose the terminal again before sending input",
          "stale_server",
        );
        this.#failQueuedActions(this.#targetError);
        this.#showStatus("Server restarted — return to Mission Control and choose the terminal again");
      } else {
        this.#showStatus("Waiting for terminal to restore…");
      }
      return;
    }
    this.#targetError = undefined;
    const changed = !previous
      || previous.serverGeneration !== target.serverGeneration
      || previous.pane.pane_id !== target.pane.pane_id
      || previous.pane.terminal_id !== target.pane.terminal_id;
    if (!changed) return;
    this.#reconnectRetry = 0;
    if (this.#stream) this.#disconnectStream(this.#streamAttempt, "Terminal identity changed");
    else {
      this.#streamAttempt += 1;
      if (this.#reconnectTimer) clearTimeout(this.#reconnectTimer);
      this.#reconnectTimer = undefined;
      this.#scheduleReconnect(true);
    }
  }

  updateTitles(_paneIds: string[]): void {
    if (this.#destroyed) return;
    if (!this.#desktopNavigation.matches && !this.#navigationOpen) return;
    this.#updateSidebar();
  }

  updateSidebarLayout(): void {
    const { workspaces, panes } = this.sidebarControls.collapsed;
    this.root.classList.toggle("workspaces-collapsed", workspaces);
    this.root.classList.toggle("panes-collapsed", panes);
    this.#workspaces.setCollapsed(workspaces);
    this.#sidebar.setCollapsed(panes);
  }

  closeNavigation(): void {
    this.#closeNavigation();
  }

  #placePaneSidebar(): void {
    const parent = this.#desktopNavigation.matches ? this.root : this.#navigation;
    if (this.#sidebar.root.parentElement !== parent) parent.append(this.#sidebar.root);
  }

  #updateSidebar(options?: TerminalPaneOption[]): void {
    this.#sidebar.update(this.#desktopNavigation.matches || this.#navigationOpen ? options ?? this.paneOptions() : [], this.#target?.pane);
  }

  #updateWorkspaces(): void {
    this.#workspaces.update((this.#desktopNavigation.matches || this.#navigationOpen) && this.#snapshot.session === this.#targetTracker.session
      ? terminalWorkspaceOptions(this.#snapshot, this.#target?.pane)
      : []);
  }

  #openNavigation(): void {
    if (this.#destroyed || this.#desktopNavigation.matches) return;
    this.#navigationOpen = true;
    this.#input?.blur();
    this.#header.inert = true;
    this.#main.inert = true;
    if (this.#input) this.#input.element.inert = true;
    this.root.classList.add("pane-navigation-open");
    this.#navigation.setAttribute("role", "dialog");
    this.#navigation.setAttribute("aria-modal", "true");
    this.#paneSelector?.setAttribute("aria-expanded", "true");
    this.#updateSidebar();
    this.#updateWorkspaces();
    this.#sidebar.closeButton.focus({ preventScroll: true });
  }

  #closeNavigation(restoreFocus = false): void {
    if (!this.#navigationOpen) return;
    this.#navigationOpen = false;
    this.root.classList.remove("pane-navigation-open");
    this.#navigation.removeAttribute("role");
    this.#navigation.removeAttribute("aria-modal");
    this.#header.inert = false;
    this.#main.inert = false;
    if (this.#input) this.#input.element.inert = false;
    this.#paneSelector?.setAttribute("aria-expanded", "false");
    this.#updateSidebar();
    this.#updateWorkspaces();
    if (restoreFocus && !this.#desktopNavigation.matches) this.#paneSelector?.focus({ preventScroll: true });
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
      if (pending.text === this.#renderedPaint?.text
        && pending.cursorOffset === this.#renderedPaint.cursorOffset
        && pending.cursorPadding === this.#renderedPaint.cursorPadding) return;
      const followTail = this.#followTail;
      const anchor = followTail ? undefined : captureTerminalScroll(this.#output, this.#content);
      updateTerminalFrame(this.#content, terminalFrameParts(pending.text, pending.cursorOffset, pending.cursorPadding));
      this.#renderedPaint = pending;
      if (followTail) this.#scrollToLatest();
      else if (anchor) restoreTerminalScroll(this.#output, this.#content, anchor);
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
    if (this.#targetError) throw this.#targetError;
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

  // Ask once the layout has settled: resizes and keyboard changes arrive in bursts.
  #scheduleViewport(force = false): void {
    if (!this.viewportSizing || this.#destroyed) return;
    if (this.#viewportTimer !== undefined) clearTimeout(this.#viewportTimer);
    this.#viewportTimer = setTimeout(() => {
      this.#viewportTimer = undefined;
      void this.#requestViewport(force);
    }, 150);
  }

  async #requestViewport(force: boolean): Promise<void> {
    const stream = this.#stream;
    if (!stream || this.#destroyed) return;
    const style = getComputedStyle(this.#output);
    const cell = this.#cellProbe.getBoundingClientRect();
    const wanted = terminalViewportFor(
      this.#output.clientWidth - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight),
      this.#output.clientHeight - parseFloat(style.paddingTop) - parseFloat(style.paddingBottom),
      cell.width / 10,
      cell.height,
    );
    if (!wanted || (!force && sameViewport(wanted, this.#lastViewport))) return;
    this.#lastViewport = wanted;
    try {
      // Sent straight to the stream: a refusal is not an input failure and must
      // not queue, scroll, or reconnect anything.
      await stream.action("set_viewport", { cols: wanted.cols, rows: wanted.rows });
    } catch {
      // A native client owns the size, or the stream went away. The frame is
      // shown as served; the next connect, resize, or focus asks again.
      this.#lastViewport = undefined;
    }
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

  #scheduleReconnect(immediate = false): void {
    if (this.#destroyed || this.#stream || this.#reconnectTimer) return;
    const delay = immediate ? 0 : Math.min(5_000, 150 * 2 ** Math.min(this.#reconnectRetry++, 5));
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
