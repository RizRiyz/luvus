import { element } from "./dom.js";

export type SidebarSide = "workspaces" | "panes";

export interface SidebarControls {
  collapsed: Record<SidebarSide, boolean>;
  toggle: (side: SidebarSide) => void;
  refresh: () => void;
  footer: () => HTMLElement;
}

export function navigationBrand(onOverview: () => void): HTMLButtonElement {
  return element("button", {
    className: "terminal-workspace-brand",
    attrs: { type: "button", "aria-label": "Luvus overview", "data-view-key": "navigation-overview" },
    on: { click: onOverview },
  }, element("img", { attrs: { src: "/mark.svg", alt: "" } }), element("span", { text: "Luvus" }));
}

export function navigationDot(label: string, controls: string, onClick: () => void): HTMLButtonElement {
  return element("button", {
    className: "terminal-pane-selector",
    attrs: { type: "button", "aria-label": label, title: label, "aria-controls": controls },
    on: { click: onClick },
  }, element("span", { className: "terminal-pane-dot", attrs: { "aria-hidden": "true" } }));
}

/** Keep keyboard navigation inside a visible drawer or device dialog. */
export function trapNavigationFocus(event: KeyboardEvent, root: HTMLElement): void {
  if (event.key !== "Tab") return;
  const controls = Array.from(root.querySelectorAll<HTMLElement>("button:not(:disabled), input:not(:disabled), select:not(:disabled), textarea:not(:disabled), [tabindex]"))
    .filter((control) => control.getClientRects().length > 0 && control.getAttribute("tabindex") !== "-1");
  const first = controls[0];
  const last = controls.at(-1);
  const focused = document.activeElement;
  if (event.shiftKey && focused === first) {
    event.preventDefault();
    last?.focus();
  } else if (!event.shiftKey && focused === last) {
    event.preventDefault();
    first?.focus();
  }
}

export function sidebarToggle(side: SidebarSide, controls: string, onClick: () => void): HTMLButtonElement {
  const arrow = icon([side === "workspaces" ? "m14 7-5 5 5 5" : "m10 7 5 5-5 5"]);
  arrow.classList.add("sidebar-collapse-arrow");
  const control = element("button", {
    className: "sidebar-collapse-toggle",
    attrs: { type: "button", "aria-controls": controls, "data-view-key": `sidebar-toggle:${side}` },
    on: { click: onClick },
  }, arrow);
  updateSidebarToggle(control, side, false);
  return control;
}

export function updateSidebarToggle(control: HTMLButtonElement, side: SidebarSide, collapsed: boolean): void {
  const label = `${collapsed ? "Expand" : "Collapse"} ${side} sidebar`;
  control.setAttribute("aria-label", label);
  control.setAttribute("aria-expanded", String(!collapsed));
  control.title = label;
}

export function refreshButton(onClick: () => void, disabled = false): HTMLButtonElement {
  return element("button", {
    className: "mission-nav-button sidebar-refresh",
    attrs: { type: "button", "aria-label": "Refresh live stats", title: "Refresh live stats", "data-view-key": "sidebar-refresh", ...(disabled ? { disabled: "" } : {}) },
    on: { click: onClick },
  }, icon(["M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8", "M3 3v5h5"]));
}

function icon(paths: string[]): SVGSVGElement {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("class", "mission-icon");
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("aria-hidden", "true");
  for (const data of paths) {
    const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
    path.setAttribute("d", data);
    path.setAttribute("fill", "none");
    path.setAttribute("stroke", "currentColor");
    path.setAttribute("stroke-width", "1.5");
    path.setAttribute("stroke-linecap", "round");
    path.setAttribute("stroke-linejoin", "round");
    svg.append(path);
  }
  return svg;
}
