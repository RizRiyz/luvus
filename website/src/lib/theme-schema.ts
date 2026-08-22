export type ThemeFieldId =
  | 'crust' | 'mantle' | 'base' | 'surface0' | 'surface1'
  | 'overlay0' | 'overlay1' | 'subtext0' | 'subtext1' | 'text'
  | 'accent' | 'sel_bg' | 'border' | 'border_focus'
  | 'green' | 'mint' | 'amber' | 'coral';

export type ThemePalette = Record<ThemeFieldId, string>;

export const THEME_FILE_VERSION = '1.0.0';
export const THEME_MIN_LUVUS = '0.12.0';

export type ThemeField = {
  id: ThemeFieldId;
  label: string;
  group: 'Surfaces' | 'Text' | 'Interaction' | 'Agent states';
  css: string;
  help: string;
};

/**
 * The public theme-file contract, in the same semantic order as Rust's Theme.
 * The /themes editor, its preview, and TOML exporter all iterate this list so a
 * field cannot exist in one surface and silently disappear from another.
 */
export const THEME_FIELDS: ThemeField[] = [
  { id: 'crust', label: 'Crust', group: 'Surfaces', css: '--bg', help: 'Outer background and deepest chrome' },
  { id: 'mantle', label: 'Mantle', group: 'Surfaces', css: '--bg2', help: 'Pane and terminal background' },
  { id: 'base', label: 'Base', group: 'Surfaces', css: '--base', help: 'Sidebars and raised regions' },
  { id: 'surface0', label: 'Surface 0', group: 'Surfaces', css: '--surface', help: 'Selected tabs and quiet controls' },
  { id: 'surface1', label: 'Surface 1', group: 'Surfaces', css: '--line', help: 'Rules, dividers, and raised controls' },
  { id: 'overlay0', label: 'Overlay 0', group: 'Surfaces', css: '--overlay0', help: 'Muted rules and disabled content' },
  { id: 'overlay1', label: 'Overlay 1', group: 'Surfaces', css: '--overlay1', help: 'Stronger secondary decoration' },
  { id: 'subtext0', label: 'Subtext 0', group: 'Text', css: '--sub', help: 'Quiet metadata and inactive labels' },
  { id: 'subtext1', label: 'Subtext 1', group: 'Text', css: '--sub2', help: 'Secondary readable text' },
  { id: 'text', label: 'Text', group: 'Text', css: '--text', help: 'Primary foreground text' },
  { id: 'accent', label: 'Accent', group: 'Interaction', css: '--accent', help: 'Focus, active tabs, links, and key hints' },
  { id: 'sel_bg', label: 'Selection', group: 'Interaction', css: '--sel', help: 'Selected row and text background' },
  { id: 'border', label: 'Border', group: 'Interaction', css: '--border', help: 'Unfocused pane frame' },
  { id: 'border_focus', label: 'Focus border', group: 'Interaction', css: '--border-focus', help: 'Focused pane frame' },
  { id: 'green', label: 'Idle', group: 'Agent states', css: '--green', help: 'Idle and successful state' },
  { id: 'mint', label: 'Done', group: 'Agent states', css: '--mint', help: 'Completed agent state' },
  { id: 'amber', label: 'Working', group: 'Agent states', css: '--amber', help: 'Working agent state' },
  { id: 'coral', label: 'Blocked', group: 'Agent states', css: '--coral', help: 'Blocked, destructive, and error state' },
];

export const THEME_GROUPS = ['Surfaces', 'Text', 'Interaction', 'Agent states'] as const;

/** A complete example draft offered by the editor, not a bundled Luvus theme. */
export const WARM_COPPER: ThemePalette = {
  crust: '#100b08',
  mantle: '#1c1210',
  base: '#281a15',
  surface0: '#33221b',
  surface1: '#4a3024',
  overlay0: '#765241',
  overlay1: '#9c725c',
  subtext0: '#c19a82',
  subtext1: '#ddc0a9',
  text: '#f5e5d4',
  accent: '#e08b57',
  sel_bg: '#50301f',
  border: '#694737',
  border_focus: '#b8784f',
  green: '#a8c66c',
  mint: '#79d1b0',
  amber: '#e5ad54',
  coral: '#e4776b',
};

export type MakerTheme = {
  id: string;
  name: string;
  appearance: 'dark' | 'light';
  description: string;
  colors: ThemePalette;
};

/** Reviewed community palettes, also offered as complete editable examples on /themes. */
export const MAKER_THEMES: MakerTheme[] = [
  {
    id: 'aurora-circuit', name: 'Aurora Circuit', appearance: 'dark',
    description: 'Deep teal surfaces with a bright mint signal.',
    colors: {
      crust: '#071019', mantle: '#0c1822', base: '#122532', surface0: '#172f3c',
      surface1: '#214454', overlay0: '#3c6372', overlay1: '#568191', subtext0: '#82a8b2',
      subtext1: '#add0d5', text: '#e7fbfa', accent: '#57f2c4', sel_bg: '#16483f',
      border: '#295664', border_focus: '#4eb99f', green: '#78d38b', mint: '#57e0ca',
      amber: '#f2c96d', coral: '#f07178',
    },
  },
  {
    id: 'cinder-bloom', name: 'Cinder Bloom', appearance: 'dark',
    description: 'Dark rose surfaces with a vivid pink accent.',
    colors: {
      crust: '#140b10', mantle: '#211018', base: '#301722', surface0: '#3b1d29',
      surface1: '#512939', overlay0: '#754457', overlay1: '#9b6578', subtext0: '#bd8797',
      subtext1: '#dbacb8', text: '#f8e4e8', accent: '#ff7aa8', sel_bg: '#572038',
      border: '#693247', border_focus: '#b85c7c', green: '#a6cf72', mint: '#76d8b0',
      amber: '#f5b65d', coral: '#f0606f',
    },
  },
  {
    id: 'violet-static', name: 'Violet Static', appearance: 'dark',
    description: 'Layered violet surfaces with a cool lavender accent.',
    colors: {
      crust: '#0b0918', mantle: '#141128', base: '#201a3b', surface0: '#292147',
      surface1: '#3a2f5d', overlay0: '#5c4b82', overlay1: '#7968a0', subtext0: '#9f91bd',
      subtext1: '#c2b8d7', text: '#f0ecfa', accent: '#a98cff', sel_bg: '#362a61',
      border: '#493c70', border_focus: '#816dc2', green: '#8bcf8a', mint: '#68d8c2',
      amber: '#e8b65c', coral: '#ef728b',
    },
  },
  {
    id: 'pine-signal', name: 'Pine Signal', appearance: 'dark',
    description: 'Forest-green surfaces with a fresh lime accent.',
    colors: {
      crust: '#07110c', mantle: '#0d1c14', base: '#14291e', surface0: '#193528',
      surface1: '#244b38', overlay0: '#3a6a51', overlay1: '#58866c', subtext0: '#82a993',
      subtext1: '#accbbb', text: '#e5f3e9', accent: '#9ddb72', sel_bg: '#284d31',
      border: '#315e45', border_focus: '#66a777', green: '#8dcc74', mint: '#60cfaa',
      amber: '#dfb85e', coral: '#dc6d67',
    },
  },
  {
    id: 'paper-cobalt', name: 'Paper Cobalt', appearance: 'light',
    description: 'Cool light paper with a focused cobalt accent.',
    colors: {
      crust: '#f7f8fc', mantle: '#eff2f8', base: '#e5eaf4', surface0: '#d9e0ee',
      surface1: '#c7d1e3', overlay0: '#9caac1', overlay1: '#74839d', subtext0: '#59677c',
      subtext1: '#3e4b5d', text: '#222c3a', accent: '#245cc7', sel_bg: '#ccdaf5',
      border: '#b3bfd2', border_focus: '#657895', green: '#397d54', mint: '#167f7a',
      amber: '#a56813', coral: '#b7354a',
    },
  },
  {
    id: 'solar-ash', name: 'Solar Ash', appearance: 'dark',
    description: 'Charcoal surfaces with a warm solar-yellow accent.',
    colors: {
      crust: '#0e0e0c', mantle: '#181815', base: '#24231e', surface0: '#2c2a24',
      surface1: '#403c31', overlay0: '#635c49', overlay1: '#847b65', subtext0: '#aba18a',
      subtext1: '#cec3aa', text: '#f3ead6', accent: '#f4d35e', sel_bg: '#4c431e',
      border: '#57513f', border_focus: '#9a8d67', green: '#a6c96a', mint: '#68cbaa',
      amber: '#e9a94f', coral: '#df665c',
    },
  },
  {
    id: 'neon-orchard', name: 'Neon Orchard', appearance: 'dark',
    description: 'Dark plum surfaces with an electric orchard-green accent.',
    colors: {
      crust: '#100916', mantle: '#1b1024', base: '#281733', surface0: '#352043',
      surface1: '#4a2d5c', overlay0: '#68457c', overlay1: '#87639a', subtext0: '#aa8eb8',
      subtext1: '#ceb9d6', text: '#f6ebf8', accent: '#b9f45d', sel_bg: '#3d5120',
      border: '#553867', border_focus: '#8ab448', green: '#8fd15e', mint: '#5edbb7',
      amber: '#efbd59', coral: '#f06b82',
    },
  },
  {
    id: 'desert-paper', name: 'Desert Paper', appearance: 'light',
    description: 'Warm light paper with a restrained clay accent.',
    colors: {
      crust: '#fcf8ee', mantle: '#f4ecdc', base: '#e9ddc8', surface0: '#ddceb4',
      surface1: '#cbb996', overlay0: '#a18d69', overlay1: '#7d6a4e', subtext0: '#62543f',
      subtext1: '#493d2e', text: '#2d251c', accent: '#9f3f1c', sel_bg: '#f1d0b9',
      border: '#c6b18f', border_focus: '#8e7655', green: '#557a3a', mint: '#2f8070',
      amber: '#a96e17', coral: '#a73535',
    },
  },
];
