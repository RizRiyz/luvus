// Sizing a terminal for this screen: the control stream may ask the server for
// a viewport with `set_viewport` when the server advertises that action. The
// server applies it only while no native client renders, so the request is a
// wish, never an override; a refusal is expected whenever Luvus is open on the
// desktop and the browser then keeps showing the frame it is given.

export interface TerminalViewport {
  cols: number;
  rows: number;
}

/** Protocol limits for `set_viewport`; a smaller screen is not worth a request. */
const MIN_COLS = 20;
const MIN_ROWS = 4;
const MAX_COLS = 500;
const MAX_ROWS = 300;

/** Only a control stream can set a viewport, and only on a server that names the action. */
export function viewportSizingAvailable(capabilities: readonly string[] | undefined, control: boolean): boolean {
  return control && (capabilities?.includes("set_viewport") ?? false);
}

/**
 * How many cells fit the output area, from one measured cell. Fractional
 * cells are dropped so a full row never wraps; sub-minimum areas get nothing,
 * since a handful of columns would make any program unusable.
 */
export function terminalViewportFor(width: number, height: number, cellWidth: number, cellHeight: number): TerminalViewport | undefined {
  if (!(width > 0) || !(height > 0) || !(cellWidth > 0) || !(cellHeight > 0)) return undefined;
  const cols = Math.min(MAX_COLS, Math.floor(width / cellWidth + 0.01));
  const rows = Math.min(MAX_ROWS, Math.floor(height / cellHeight + 0.01));
  if (cols < MIN_COLS || rows < MIN_ROWS) return undefined;
  return { cols, rows };
}

export function sameViewport(a: TerminalViewport | undefined, b: TerminalViewport | undefined): boolean {
  return a?.cols === b?.cols && a?.rows === b?.rows;
}
