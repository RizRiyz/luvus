export interface AnsiRun {
  text: string;
  style?: Partial<CSSStyleDeclaration>;
}

interface State {
  foreground: string | undefined;
  background: string | undefined;
  bold: boolean;
  dim: boolean;
  italic: boolean;
  underline: boolean;
  reverse: boolean;
  hidden: boolean;
  strike: boolean;
}

const PALETTE = [
  "#181922", "#ed8f9e", "#9ed68a", "#ead778", "#90b9f2", "#c6a0f6", "#7bdff2", "#d9d7e7",
  "#676579", "#f3a7b4", "#b5e8a4", "#f1e38f", "#a9c9f7", "#d5b7fa", "#9ae8f5", "#f4f1ff",
] as const;

export function parseAnsi(input: string): AnsiRun[] {
  const runs: AnsiRun[] = [];
  const state = freshState();
  const sgr = /\x1b\[([0-9;]*)m/g;
  let offset = 0;
  for (const match of input.matchAll(sgr)) {
    const index = match.index ?? offset;
    appendRun(runs, sanitize(input.slice(offset, index)), state);
    applyCodes(state, match[1] ? match[1].split(";").map(Number) : [0]);
    offset = index + match[0].length;
  }
  appendRun(runs, sanitize(input.slice(offset)), state);
  return runs;
}

function freshState(): State {
  return { foreground: undefined, background: undefined, bold: false, dim: false, italic: false, underline: false, reverse: false, hidden: false, strike: false };
}

function reset(state: State): void {
  Object.assign(state, freshState(), { foreground: undefined, background: undefined });
}

function sanitize(text: string): string {
  return text.replace(/\r\n?/g, "\n").replace(/[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]/g, "");
}

function appendRun(runs: AnsiRun[], text: string, state: State): void {
  if (!text) return;
  const style = cssStyle(state);
  runs.push({ text, ...(style ? { style } : {}) });
}

function cssStyle(state: State): Partial<CSSStyleDeclaration> | undefined {
  let foreground = state.foreground;
  let background = state.background;
  if (state.reverse) [foreground, background] = [background ?? "#d9d7e7", foreground ?? "#111219"];
  const style: Partial<CSSStyleDeclaration> = {};
  if (foreground) style.color = foreground;
  if (background) style.backgroundColor = background;
  if (state.bold) style.fontWeight = "700";
  if (state.dim) style.opacity = "0.65";
  if (state.italic) style.fontStyle = "italic";
  if (state.underline || state.strike) style.textDecoration = [state.underline ? "underline" : "", state.strike ? "line-through" : ""].filter(Boolean).join(" ");
  if (state.hidden) style.visibility = "hidden";
  return Object.keys(style).length ? style : undefined;
}

function applyCodes(state: State, codes: number[]): void {
  for (let index = 0; index < codes.length; index += 1) {
    const code = Number.isFinite(codes[index]) ? codes[index]! : 0;
    if (code === 0) reset(state);
    else if (code === 1) state.bold = true;
    else if (code === 2) state.dim = true;
    else if (code === 3) state.italic = true;
    else if (code === 4) state.underline = true;
    else if (code === 7) state.reverse = true;
    else if (code === 8) state.hidden = true;
    else if (code === 9) state.strike = true;
    else if (code === 22) { state.bold = false; state.dim = false; }
    else if (code === 23) state.italic = false;
    else if (code === 24) state.underline = false;
    else if (code === 27) state.reverse = false;
    else if (code === 28) state.hidden = false;
    else if (code === 29) state.strike = false;
    else if (code >= 30 && code <= 37) state.foreground = indexedColor(code - 30);
    else if (code >= 90 && code <= 97) state.foreground = indexedColor(code - 90 + 8);
    else if (code === 39) state.foreground = undefined;
    else if (code >= 40 && code <= 47) state.background = indexedColor(code - 40);
    else if (code >= 100 && code <= 107) state.background = indexedColor(code - 100 + 8);
    else if (code === 49) state.background = undefined;
    else if (code === 38 || code === 48) {
      const color = extendedColor(codes, index + 1);
      if (color) {
        if (code === 38) state.foreground = color.value;
        else state.background = color.value;
        index += color.consumed;
      }
    }
  }
}

function extendedColor(codes: number[], index: number): { value: string; consumed: number } | undefined {
  if (codes[index] === 5 && Number.isInteger(codes[index + 1])) return { value: indexedColor(codes[index + 1]!), consumed: 2 };
  if (codes[index] === 2 && [codes[index + 1], codes[index + 2], codes[index + 3]].every(Number.isInteger)) {
    const [red, green, blue] = codes.slice(index + 1, index + 4).map(clampByte);
    return { value: `rgb(${red}, ${green}, ${blue})`, consumed: 4 };
  }
  return undefined;
}

function indexedColor(index: number): string {
  const value = Math.max(0, Math.min(255, index));
  if (value < 16) return PALETTE[value]!;
  if (value < 232) {
    const offset = value - 16;
    const channel = (part: number) => part === 0 ? 0 : 55 + part * 40;
    return `rgb(${channel(Math.floor(offset / 36))}, ${channel(Math.floor((offset % 36) / 6))}, ${channel(offset % 6)})`;
  }
  const gray = 8 + (value - 232) * 10;
  return `rgb(${gray}, ${gray}, ${gray})`;
}

function clampByte(value: number): number {
  return Math.max(0, Math.min(255, value));
}
