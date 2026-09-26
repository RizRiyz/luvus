export function element<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  options: { className?: string; text?: string; attrs?: Record<string, string>; on?: Record<string, EventListener> } = {},
  ...children: Array<Node | string | undefined>
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  if (options.className) node.className = options.className;
  if (options.text !== undefined) node.textContent = options.text;
  for (const [name, value] of Object.entries(options.attrs ?? {})) node.setAttribute(name, value);
  for (const [name, listener] of Object.entries(options.on ?? {})) node.addEventListener(name, listener);
  for (const child of children) {
    if (child !== undefined) node.append(child);
  }
  return node;
}

export function button(label: string, className: string, onClick: () => void): HTMLButtonElement {
  return element("button", { className, text: label, attrs: { type: "button" }, on: { click: onClick } });
}
