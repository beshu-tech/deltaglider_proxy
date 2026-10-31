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

interface ThemeContextValue {
  isDark: boolean;
  toggleTheme: () => void;
  colors: ColorTokens;
}

export const ThemeContext = createContext<ThemeContextValue>({
  isDark: true,
  toggleTheme: () => {},
  colors: darkColors,
});

export function useColors() {
  const { colors } = useContext(ThemeContext);
  return colors;
}

export function useTheme() {
  return useContext(ThemeContext);
}
