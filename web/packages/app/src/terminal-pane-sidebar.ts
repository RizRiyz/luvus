import type { PaneSnapshot } from "@luvus/uhp-client";
import { displayText, paneStateClass } from "./dashboard-agents.js";
import { element } from "./dom.js";
import { sidebarToggle, updateSidebarToggle } from "./sidebar-controls.js";
import { filterTerminalPanes, terminalPaneLabel, type TerminalPaneOption } from "./terminal-pane-options.js";

export interface TerminalPaneFilter {
  showShells: boolean;
  onChange: (showShells: boolean) => void;
}

type PaneRow = {
  option: TerminalPaneOption;
  button: HTMLButtonElement;
  state: HTMLElement;
  title: HTMLElement;
  agent: HTMLElement;
  context: HTMLElement;
};

/** Shared navigation; live updates retain row nodes, focus, and list scroll. */
export class TerminalPaneSidebar {
  readonly root = element("aside", { className: "terminal-sidebar", attrs: { id: "terminal-pane-navigation", "aria-label": "Agents and panes" } });
  readonly closeButton: HTMLButtonElement;
  readonly #toggle: HTMLButtonElement;
  #heading = element("h2", { text: "Agents" });
  #count = element("span", { className: "terminal-sidebar-count" });
  #list = element("nav", { className: "terminal-sidebar-list", attrs: { "aria-label": "Switch terminal pane", "data-scroll-key": "pane-sidebar" } });
  #empty = element("p", { className: "terminal-sidebar-empty" });
  #filters = new Map<boolean, HTMLButtonElement>();
  #rows = new Map<string, PaneRow>();
  #options: TerminalPaneOption[] = [];
  #selected: PaneSnapshot | undefined;
  #showShells: boolean;

  constructor(private readonly onSelectPane: (pane: PaneSnapshot) => void, filter: TerminalPaneFilter, onClose: () => void, onToggle: () => void) {
    this.#showShells = filter.showShells;
    this.#toggle = sidebarToggle("panes", this.root.id, onToggle);
    const filters = element("div", { className: "agent-filters terminal-sidebar-filters", attrs: { role: "group", "aria-label": "Filter terminal panes" } });
    for (const showShells of [false, true]) {
      const control = element("button", {
        className: "agent-filter",
        text: showShells ? "All panes" : "Active agents",
        attrs: { type: "button", "data-view-key": `pane-filter:${showShells}`, title: showShells ? "Include shell panes" : "Show detected agents, including idle and waiting agents" },
        on: { click: () => {
          this.#showShells = showShells;
          filter.onChange(showShells);
          this.update(this.#options, this.#selected);
        } },
      });
      this.#filters.set(showShells, control);
      filters.append(control);
    }
    this.closeButton = element("button", {
      className: "terminal-pane-selector terminal-sidebar-close",
      attrs: { type: "button", "aria-label": "Close pane list", title: "Close pane list", "aria-expanded": "true", "aria-controls": "terminal-pane-navigation" },
      on: { click: onClose },
    }, element("span", { className: "terminal-pane-dot", attrs: { "aria-hidden": "true" } }));
    this.#list.append(this.#empty);
    this.root.append(
      element("header", { className: "terminal-sidebar-heading" }, this.#heading, this.#count, this.#toggle),
      element("div", { className: "terminal-sidebar-toolbar" }, filters, this.closeButton),
      this.#list,
    );
  }

  setCollapsed(collapsed: boolean): void {
    this.root.classList.toggle("collapsed", collapsed);
    updateSidebarToggle(this.#toggle, "panes", collapsed);
  }

  update(options: TerminalPaneOption[], selected: PaneSnapshot | undefined, showShells = this.#showShells): void {
    this.#showShells = showShells;
    this.#options = options;
    this.#selected = selected;
    const visible = filterTerminalPanes(options, this.#showShells);
    this.#heading.textContent = this.#showShells ? "Panes" : "Agents";
    this.#count.textContent = String(visible.length);
    this.#count.setAttribute("aria-label", `${visible.length} ${this.#showShells ? "terminal panes" : "active agents"}`);
    for (const [showShells, control] of this.#filters) control.setAttribute("aria-pressed", String(showShells === this.#showShells));

    const ids = new Set(visible.map(({ pane }) => pane.pane_id));
    for (const [id, row] of this.#rows) {
      if (ids.has(id)) continue;
      row.button.remove();
      this.#rows.delete(id);
    }
    let next = this.#list.firstChild;
    for (const option of visible) {
      const id = option.pane.pane_id;
      const row = this.#rows.get(id) ?? this.#createRow(option);
      row.option = option;
      const active = option.pane.pane_id === selected?.pane_id && option.pane.terminal_id === selected?.terminal_id;
      const state = option.pane.is_agent === true ? displayText(option.pane.agent_status, "unknown") : "shell";
      const statusClass = paneStateClass(state);
      row.button.className = `terminal-sidebar-pane ${statusClass}${active ? " active" : ""}`;
      row.button.title = `${terminalPaneLabel(option)} · ${state}\n${option.context} · ${option.path}`;
      row.button.setAttribute("aria-label", `${terminalPaneLabel(option)}, ${option.context}, ${state}`);
      if (active) row.button.setAttribute("aria-current", "true");
      else row.button.removeAttribute("aria-current");
      setText(row.state, state.toLowerCase());
      setText(row.title, option.title);
      row.title.hidden = !option.title;
      setText(row.agent, option.agentName);
      setText(row.context, `${option.title ? "| " : ""}${option.context}`);
      // Do not detach unchanged buttons while the user is pressing or focusing them.
      if (row.button !== next) this.#list.insertBefore(row.button, next);
      next = row.button.nextSibling;
    }
    this.#empty.hidden = visible.length > 0;
    setText(this.#empty, this.#showShells ? "No terminal panes available." : "No active agents. Choose All panes to show shells.");
  }

  #createRow(option: TerminalPaneOption): PaneRow {
    const state = element("span", { className: "terminal-sidebar-state" });
    const title = element("span", { className: "terminal-sidebar-title" });
    const agent = element("span", { className: "terminal-sidebar-agent" });
    const context = element("span", { className: "terminal-sidebar-context" });
    const control = element("button", {
      className: "terminal-sidebar-pane",
      attrs: { type: "button", "data-sidebar-pane": option.pane.pane_id, "data-view-key": `sidebar-pane:${option.pane.pane_id}` },
      on: { click: () => this.onSelectPane(row.option.pane) },
    },
    element("span", { className: "terminal-sidebar-copy" },
      element("strong", { className: "terminal-sidebar-label" },
        element("span", { className: "terminal-sidebar-dot", attrs: { "aria-hidden": "true" } }), state, agent),
      element("span", { className: "terminal-sidebar-detail" }, title, context),
    ));
    const row = { option, button: control, state, title, agent, context };
    this.#rows.set(option.pane.pane_id, row);
    return row;
  }
}

function setText(node: HTMLElement, text: string): void {
  if (node.textContent !== text) node.textContent = text;
}
