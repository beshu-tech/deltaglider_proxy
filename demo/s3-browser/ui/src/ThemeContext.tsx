import { createContext, useContext } from 'react';

// Dark colors — deep navy instrument panel
export const darkColors = {
  BG_BASE: '#080c14',
  BG_SIDEBAR: '#0b1120',
  BG_CARD: '#111827',
  BG_ELEVATED: '#162032',
  BORDER: '#1e2d45',
  TEXT_PRIMARY: '#e4e9f2',
  TEXT_SECONDARY: '#8b99b0',
  TEXT_MUTED: '#9aa5b5',
  TEXT_FAINT: '#88a0b8',
  ACCENT_BLUE: '#2dd4bf',     // teal as primary accent
  ACCENT_BLUE_LIGHT: '#5eead4',
  ACCENT_GREEN: '#34d399',
  ACCENT_RED: '#fb7185',
  ACCENT_PURPLE: '#a78bfa',
  ACCENT_AMBER: '#fbbf24',
  // Text and icons ON any ACCENT_* fill (primary button, skip link, a ticked
  // chip): 4.5:1 on every accent of its theme (contrast.test.ts).
  ON_ACCENT: '#0b1120',
  // ── Analytics "money shot" tokens (visual-craft pass) ──────────────
  // Palette story: green = saved (luminous), teal-ink = kept (dense).
  HERO_NUM_GRADIENT: 'linear-gradient(150deg, #6ee7b7 0%, #34d399 48%, #2dd4bf 100%)',
  BAR_KEPT: 'linear-gradient(180deg, #115e59 0%, #0f766e 100%)',
  BAR_SAVED: 'linear-gradient(180deg, #4ade80 0%, #34d399 55%, #10b981 100%)',
  BAR_TRACK: '#0b1120',
  INSET_SHADOW: 'inset 0 1px 3px rgba(0,0,0,0.45)',
  GLOW_GREEN: 'rgba(52,211,153,0.35)',
  ELEV_SHADOW: '0 1px 3px rgba(0,0,0,0.35), 0 8px 24px -12px rgba(0,0,0,0.5)',
  // Anything that floats over the page: popups, the dirty bar, the bulk bar.
  FLOAT_SHADOW: '0 8px 24px rgba(0,0,0,0.35)',
  // The top inner highlight of a dashboard card.
  CARD_HIGHLIGHT: 'rgba(255,255,255,0.04)',
  KEPT_TEXT: '#5eead4',
  // Text on the bars: 4.5:1 on every stop of BAR_KEPT / BAR_SAVED (contrast.test.ts).
  ON_KEPT: '#ffffff',
  ON_SAVED: '#0b1120',
};

// Light colors — clean, high-contrast
export const lightColors: ColorTokens = {
  BG_BASE: '#f5f7fa',
  BG_SIDEBAR: '#edf0f5',
  BG_CARD: '#ffffff',
  BG_ELEVATED: '#ffffff',
  BORDER: '#d5dbe5',
  TEXT_PRIMARY: '#0c1629',
  TEXT_SECONDARY: '#475569',
  TEXT_MUTED: '#55627a',
  TEXT_FAINT: '#5f6c80',
  ACCENT_BLUE: '#0f766e',     // darker teal: 4.5:1 on every light surface
  ACCENT_BLUE_LIGHT: '#115e59',
  // Text-grade (4.5:1 on every light surface): accents are also text.
  ACCENT_GREEN: '#047857',
  ACCENT_RED: '#be123c',
  ACCENT_PURPLE: '#7c3aed',
  ACCENT_AMBER: '#9a4a07',
  ON_ACCENT: '#ffffff',
  // ── Analytics "money shot" tokens (visual-craft pass) ──────────────
  HERO_NUM_GRADIENT: 'linear-gradient(150deg, #047857 0%, #059669 48%, #0d9488 100%)',
  BAR_KEPT: 'linear-gradient(180deg, #134e4a 0%, #0f766e 100%)',
  BAR_SAVED: 'linear-gradient(180deg, #34d399 0%, #10b981 52%, #059669 100%)',
  BAR_TRACK: '#eef2f6',
  INSET_SHADOW: 'inset 0 1px 2px rgba(12,22,41,0.08)',
  GLOW_GREEN: 'rgba(5,150,105,0.28)',
  ELEV_SHADOW: '0 1px 2px rgba(12,22,41,0.05), 0 4px 16px -8px rgba(12,22,41,0.10)',
  FLOAT_SHADOW: '0 8px 24px rgba(12,22,41,0.18)',
  CARD_HIGHLIGHT: 'rgba(255,255,255,0.7)',
  KEPT_TEXT: '#0f766e',
  ON_KEPT: '#ffffff',
  ON_SAVED: '#0c1629',
};

export type ColorTokens = typeof darkColors;

// ── Editorial: cream paper, ink, coral, sunflower, deep green ─────────
// Fraunces headlines over Manrope (themeVars.ts FONT_DISPLAY). Coral is the
// primary; the text-grade coral is deep (#b23c1b by day) because it is also
// link and label text. Paper, not glass: flat surfaces, a hairline foot and a
// long soft drop instead of borders and blur.
const editorialLightColors: ColorTokens = {
  BG_BASE: '#fbf6ee',
  BG_SIDEBAR: '#f4ecdf',
  BG_CARD: '#ffffff',
  BG_ELEVATED: '#ffffff',
  BORDER: '#e6dccd',
  TEXT_PRIMARY: '#1b1a17',
  TEXT_SECONDARY: '#4a453e',
  TEXT_MUTED: '#6b645a',
  TEXT_FAINT: '#6f675c',
  ACCENT_BLUE: '#b23c1b', // the primary slot holds coral
  ACCENT_BLUE_LIGHT: '#8f2f12',
  ACCENT_GREEN: '#17483a',
  ACCENT_RED: '#a3162f',
  ACCENT_PURPLE: '#7a3e6a',
  ACCENT_AMBER: '#875400',
  ON_ACCENT: '#fbf6ee',
  HERO_NUM_GRADIENT: 'linear-gradient(150deg, #17483a 0%, #1d5545 55%, #2a6b56 100%)',
  BAR_KEPT: 'linear-gradient(180deg, #f0c3a8 0%, #e8b08f 100%)',
  BAR_SAVED: 'linear-gradient(180deg, #2a6b56 0%, #1d5545 55%, #17483a 100%)',
  BAR_TRACK: '#f4ecdf',
  INSET_SHADOW: 'inset 0 1px 2px rgba(27,26,23,0.08)',
  GLOW_GREEN: 'rgba(23,72,58,0.22)',
  ELEV_SHADOW: '0 1px 0 #e6dccd, 0 18px 40px -24px rgba(27,26,23,0.35)',
  FLOAT_SHADOW: '0 1px 0 #e6dccd, 0 28px 50px -24px rgba(27,26,23,0.45)',
  CARD_HIGHLIGHT: 'rgba(255,255,255,0)',
  KEPT_TEXT: '#8f2f12',
  ON_KEPT: '#1b1a17',
  ON_SAVED: '#fbf6ee',
};

const editorialDarkColors: ColorTokens = {
  BG_BASE: '#141311',
  BG_SIDEBAR: '#1b1a17',
  BG_CARD: '#26241f',
  BG_ELEVATED: '#2f2c26',
  BORDER: '#3d3a33',
  TEXT_PRIMARY: '#fbf6ee',
  TEXT_SECONDARY: '#d6cfc3',
  TEXT_MUTED: '#b5ad9f',
  TEXT_FAINT: '#aaa293',
  ACCENT_BLUE: '#f58f6c',
  ACCENT_BLUE_LIGHT: '#f8a98b',
  ACCENT_GREEN: '#86c9ab',
  ACCENT_RED: '#f58a9b',
  ACCENT_PURPLE: '#cfa6e0',
  ACCENT_AMBER: '#ffc93d', // sunflower
  ON_ACCENT: '#1b1a17',
  HERO_NUM_GRADIENT: 'linear-gradient(150deg, #a8dcc4 0%, #86c9ab 50%, #5fae8b 100%)',
  BAR_KEPT: 'linear-gradient(180deg, #f0c3a8 0%, #e8b08f 100%)',
  BAR_SAVED: 'linear-gradient(180deg, #86c9ab 0%, #5fae8b 100%)',
  BAR_TRACK: '#1b1a17',
  INSET_SHADOW: 'inset 0 1px 3px rgba(0,0,0,0.45)',
  GLOW_GREEN: 'rgba(134,201,171,0.25)',
  ELEV_SHADOW: '0 18px 40px -24px rgba(0,0,0,0.6)',
  FLOAT_SHADOW: '0 28px 50px -20px rgba(0,0,0,0.7)',
  CARD_HIGHLIGHT: 'rgba(255,255,255,0)',
  KEPT_TEXT: '#f0c3a8',
  ON_KEPT: '#1b1a17',
  ON_SAVED: '#1b1a17',
};

// ── Windows XP: Luna (light) and Royale Noir (dark) ───────────────────
// Light is the Luna blue theme: the beige window face (#ece9d8), white
// content, Luna blue, the XP green and the orange hot-track. Dark is the
// Media Center "Royale Noir": black glass with bright blue. The beveled
// look (title bars, glossy buttons, Luna scrollbars) is in theme.css.
const xpLightColors: ColorTokens = {
  BG_BASE: '#ece9d8',
  BG_SIDEBAR: '#dbe5fa',
  BG_CARD: '#ffffff',
  BG_ELEVATED: '#ffffff',
  BORDER: '#aca899',
  TEXT_PRIMARY: '#000000',
  TEXT_SECONDARY: '#262626',
  TEXT_MUTED: '#4d4d4d',
  TEXT_FAINT: '#595959',
  ACCENT_BLUE: '#0054e3', // Luna blue
  ACCENT_BLUE_LIGHT: '#0a3fb0',
  ACCENT_GREEN: '#1b6f1b',
  ACCENT_RED: '#b3201a',
  ACCENT_PURPLE: '#6a3a9c',
  ACCENT_AMBER: '#8a4b00',
  ON_ACCENT: '#ffffff',
  HERO_NUM_GRADIENT: 'linear-gradient(180deg, #2f9e2f 0%, #1b6f1b 100%)',
  BAR_KEPT: 'linear-gradient(180deg, #2a6cdc 0%, #0a3fb0 100%)',
  BAR_SAVED: 'linear-gradient(180deg, #7fe07f 0%, #3dbb3d 50%, #2a9a2a 100%)',
  BAR_TRACK: '#ffffff',
  INSET_SHADOW: 'inset 1px 1px 0 #aca899',
  GLOW_GREEN: 'rgba(61,187,61,0.40)',
  ELEV_SHADOW: '1px 1px 2px rgba(0,0,0,0.28)',
  FLOAT_SHADOW: '3px 4px 8px rgba(0,0,0,0.42)',
  CARD_HIGHLIGHT: 'rgba(255,255,255,0.85)',
  KEPT_TEXT: '#0a3fb0',
  ON_KEPT: '#ffffff',
  ON_SAVED: '#000000',
};

const xpDarkColors: ColorTokens = {
  BG_BASE: '#0f1320',
  BG_SIDEBAR: '#161b2c',
  BG_CARD: '#1c2236',
  BG_ELEVATED: '#252d46',
  BORDER: '#3d4868',
  TEXT_PRIMARY: '#f2f5fc',
  TEXT_SECONDARY: '#cfd6e8',
  TEXT_MUTED: '#adb8d0',
  TEXT_FAINT: '#a1adc8',
  // AntD darkens a night seed by about 12 % for links and the primary fill: this derived colour reads 4.5:1 on the cards.
  ACCENT_BLUE: '#84b9ff',
  ACCENT_BLUE_LIGHT: '#a9d1ff',
  ACCENT_GREEN: '#6fd36f',
  ACCENT_RED: '#ff8a80',
  ACCENT_PURPLE: '#c3a2ff',
  ACCENT_AMBER: '#ffc83c', // the XP hot-track orange
  ON_ACCENT: '#0a1428',
  HERO_NUM_GRADIENT: 'linear-gradient(180deg, #9cef9c 0%, #6fd36f 55%, #3dbb3d 100%)',
  BAR_KEPT: 'linear-gradient(180deg, #b8d9ff 0%, #84b9ff 100%)',
  BAR_SAVED: 'linear-gradient(180deg, #8ceb8c 0%, #4fc94f 55%, #36ad36 100%)',
  BAR_TRACK: '#0f1320',
  INSET_SHADOW: 'inset 1px 1px 2px rgba(0,0,0,0.6)',
  GLOW_GREEN: 'rgba(111,211,111,0.30)',
  ELEV_SHADOW: '1px 1px 3px rgba(0,0,0,0.6)',
  FLOAT_SHADOW: '3px 5px 10px rgba(0,0,0,0.65)',
  CARD_HIGHLIGHT: 'rgba(255,255,255,0.06)',
  KEPT_TEXT: '#a9d1ff',
  ON_KEPT: '#0a1428',
  ON_SAVED: '#0a1428',
};

/**
 * A theme is a STYLE (the look) times a MODE (light / dark). Every style has
 * both palettes, every palette has every token (ColorTokens), and the tests
 * (contrast, hljs, chart) run over all of them. Adding a style = a palette
 * pair here, an entry in THEME_STYLES, and (only if it needs one) a
 * `[data-theme-style="<id>"]` block in theme.css.
 */
export type ThemeStyle = 'instrument' | 'editorial' | 'xp';
export const DEFAULT_THEME_STYLE: ThemeStyle = 'instrument';
export const THEME_STYLES: readonly { id: ThemeStyle; label: string; hint: string }[] = [
  { id: 'instrument', label: 'Instrument', hint: 'Navy and teal, sans-serif' },
  { id: 'editorial', label: 'Editorial', hint: 'Cream paper, serif headlines, coral' },
  { id: 'xp', label: 'Windows XP', hint: 'Luna blue, beveled buttons, Royale Noir by night' },
];
export const PALETTES: Record<ThemeStyle, { dark: ColorTokens; light: ColorTokens }> = {
  instrument: { dark: darkColors, light: lightColors },
  editorial: { dark: editorialDarkColors, light: editorialLightColors },
  xp: { dark: xpDarkColors, light: xpLightColors },
};

/** Pure: a stored value as a style (anything unknown → the default). */
export function parseThemeStyle(value: string | null | undefined): ThemeStyle {
  return THEME_STYLES.some((s) => s.id === value) ? (value as ThemeStyle) : DEFAULT_THEME_STYLE;
}

interface ThemeContextValue {
  isDark: boolean;
  toggleTheme: () => void;
  colors: ColorTokens;
  style: ThemeStyle;
  setStyle: (style: ThemeStyle) => void;
}

export const ThemeContext = createContext<ThemeContextValue>({
  isDark: true,
  toggleTheme: () => {},
  colors: darkColors,
  style: DEFAULT_THEME_STYLE,
  setStyle: () => {},
});

export function useColors() {
  const { colors } = useContext(ThemeContext);
  return colors;
}

export function useTheme() {
  return useContext(ThemeContext);
}
