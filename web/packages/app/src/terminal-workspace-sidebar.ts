import type { PaneSnapshot } from "@luvus/uhp-client";
import { element } from "./dom.js";
import { sidebarToggle, updateSidebarToggle } from "./sidebar-controls.js";
import type { TerminalWorkspaceOption } from "./terminal-pane-options.js";

type WorkspaceRow = {
  option: TerminalWorkspaceOption | undefined;
  root: HTMLElement;
  button: HTMLButtonElement;
  name: HTMLElement;
  branch: HTMLElement;
  path: HTMLElement;
};

/** Workspace navigation shared by desktop rails and the mobile drawer. */
export class TerminalWorkspaceSidebar {
  readonly root = element("aside", { className: "terminal-workspace-sidebar", attrs: { id: "terminal-workspace-navigation", "aria-label": "Workspaces" } });
  readonly #toggle: HTMLButtonElement;
  #count = element("span", { className: "terminal-sidebar-count" });
  #list = element("nav", { className: "terminal-sidebar-list", attrs: { "aria-label": "Switch workspace", "data-scroll-key": "workspace-sidebar" } });
  #empty = element("p", { className: "terminal-sidebar-empty", text: "No workspaces available." });
  #footer = element("footer", { className: "terminal-workspace-footer", attrs: { hidden: "" } });
  #rows = new Map<number, WorkspaceRow>();

  constructor(private readonly onSelectPane: (pane: PaneSnapshot) => void, onOverview: () => void, onToggle: () => void) {
    this.#toggle = sidebarToggle("workspaces", this.root.id, onToggle);
    this.#list.append(this.#empty);
    this.root.append(
      element("div", { className: "terminal-workspace-topbar" }, element("button", {
        className: "terminal-workspace-brand",
        attrs: { type: "button", "aria-label": "Luvus overview", "data-view-key": "sidebar-overview" },
        on: { click: onOverview },
      }, element("img", { attrs: { src: "/mark.svg", alt: "" } }), element("span", { text: "Luvus" })), this.#toggle),
      element("header", { className: "terminal-sidebar-heading" }, element("h2", { text: "Workspaces" }), this.#count),
      this.#list,
      this.#footer,
    );
  }

  setCollapsed(collapsed: boolean): void {
    this.root.classList.toggle("collapsed", collapsed);
    updateSidebarToggle(this.#toggle, "workspaces", collapsed);
  }

  setFooter(content: HTMLElement | undefined): void {
    this.#footer.replaceChildren(...(content ? [content] : []));
    this.#footer.hidden = !content;
  }

  update(options: TerminalWorkspaceOption[]): void {
    this.#count.textContent = String(options.length);
    const ids = new Set(options.map((workspace) => workspace.index));
    for (const [id, row] of this.#rows) {
      if (ids.has(id)) continue;
      row.root.remove();
      this.#rows.delete(id);
    }
    let next = this.#list.firstChild;
    for (const workspace of options) {
      const row = this.#rows.get(workspace.index) ?? this.#createWorkspace(workspace.index);
      row.option = workspace;
      row.root.classList.toggle("current", workspace.selected);
      row.button.disabled = !workspace.target;
      row.button.title = `${workspace.name}${workspace.branch ? ` · ${workspace.branch}` : ""}\n${workspace.path}${workspace.target ? "" : "\nNo live terminal panes in this workspace"}`;
      row.button.setAttribute("aria-label", `Open workspace ${workspace.name}${workspace.branch ? `, branch ${workspace.branch}` : ""}, ${workspace.path}`);
      if (workspace.selected) row.button.setAttribute("aria-current", "true");
      else row.button.removeAttribute("aria-current");
      setText(row.name, workspace.name);
      setText(row.branch, workspace.branch);
      row.branch.hidden = !workspace.branch;
      setText(row.path, workspace.path);
      if (row.root !== next) this.#list.insertBefore(row.root, next);
      next = row.root.nextSibling;
    }
    this.#empty.hidden = options.length > 0;
  }

  #createWorkspace(index: number): WorkspaceRow {
    const name = element("strong", { className: "terminal-workspace-name" });
    const branch = element("span", { className: "terminal-workspace-branch" });
    const path = element("span", { className: "terminal-workspace-path" });
    const button = element("button", {
      className: "terminal-workspace-label",
      attrs: { type: "button", "data-view-key": `sidebar-workspace:${index}` },
      on: { click: () => { if (row.option?.target) this.onSelectPane(row.option.target); } },
    },
    element("span", { className: "terminal-workspace-copy" },
      element("span", { className: "terminal-workspace-line" }, element("span", { className: "terminal-workspace-dot", attrs: { "aria-hidden": "true" } }), name, branch),
      path,
    ));
    const root = element("section", { className: "terminal-workspace-group", attrs: { "data-sidebar-workspace": String(index) } },
      button,
    );
    const row: WorkspaceRow = { option: undefined, root, button, name, branch, path };
    this.#rows.set(index, row);
    return row;
  }
}

function setText(node: HTMLElement, text: string): void {
  if (node.textContent !== text) node.textContent = text;
}
