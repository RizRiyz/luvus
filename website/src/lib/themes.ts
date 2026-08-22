/**
 * luvus's shipped palettes, mirroring `THEMES` and `describe()` in
 * `src/ui/theme.rs`. The colours themselves live in `../styles/themes.css`,
 * which is generated from that same Rust registry:
 *
 *   cargo test --features dev-tools emit_theme_css -- --nocapture
 *
 * Keep this list in the registry's order so the picker reads the way Settings
 * does in the app.
 */
export const THEMES: { id: string; label: string; note: string }[] = [
  { id: 'quattro-rally', label: 'quattro rally', note: 'soft dark, rally-gold accent' },
  { id: 'noir', label: 'noir', note: 'near-black, fluo-green accent' },
  { id: 'ocean', label: 'ocean', note: 'deep cmd-blue, cyan accent' },
  { id: 'dracula', label: 'dracula', note: 'indigo dark, violet accent' },
  { id: 'nord', label: 'nord', note: 'cool arctic blue-grey' },
  { id: 'sky', label: 'sky', note: 'light paper, sky-blue accent' },
  { id: 'catppuccin-mocha', label: 'catppuccin mocha', note: 'darkest Catppuccin, mauve' },
  { id: 'catppuccin-macchiato', label: 'catppuccin macchiato', note: 'softer dark Catppuccin' },
  { id: 'catppuccin-frappe', label: 'catppuccin frappe', note: 'lightest dark Catppuccin' },
  { id: 'gruvbox', label: 'gruvbox', note: 'retro warm dark, yellow accent' },
  { id: 'sunset', label: 'sunset', note: 'neon synthwave, hot-pink' },
  { id: 'homebrew', label: 'homebrew', note: 'classic green-on-black' },
  { id: 'grass', label: 'grass', note: 'green field, pale-yellow text' },
  { id: 'redsands', label: 'redsands', note: 'warm dark red, orange accent' },
  { id: 'catppuccin-latte', label: 'catppuccin latte', note: 'light Catppuccin, warm' },
  { id: 'gruvbox-light', label: 'gruvbox light', note: 'Gruvbox on cream, burnt orange' },
  { id: 'mono', label: 'mono', note: 'grayscale, no accent color' },
];

/** The palette the site opens with for a first-time visitor. */
export const DEFAULT_THEME = 'quattro-rally';

/**
 * The palettes whose surfaces are light. luvus's registry has no light/dark
 * flag — a theme is simply a set of colours — but Starlight keys a few of its
 * own rules (and Expressive Code's syntax theme) off `data-theme`, so the docs
 * have to say which side of the line each palette falls on.
 */
export const LIGHT_THEMES = ['catppuccin-latte', 'gruvbox-light', 'sky'];

/** `data-theme` for a palette: what Starlight's own light/dark rules key off. */
export const modeOf = (id: string) => (LIGHT_THEMES.includes(id) ? 'light' : 'dark');
