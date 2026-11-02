import { theme } from 'antd';
import { darkColors as D, lightColors as L, PALETTES, type ThemeStyle } from './ThemeContext';
import { FONT_MONO, FONT_UI } from './themeVars';

/**
 * Stacking: the unsaved-changes bar (StickyDirtyBar) sits over a top-level
 * Drawer (AntD zIndexPopupBase, 1000) so a rule edited in the Jobs drawer can
 * be applied, and every Modal sits over the bar, because its Review & apply
 * button opens the review modal.
 */
/**
 * THE stacking scale. Every zIndex in the app is a value of Z (source test
 * zScale.test.ts); `local` orders siblings inside one card. AntD's Drawer
 * and popups sit at 1000.
 */
export const Z = {
  local: 1,
  /** The inspector's share menu over the inspector. */
  menu: 10,
  /** The bulk action bar over the listing. */
  floating: 20,
  /** The upload drop target: under a Drawer (1000). */
  dropTarget: 999,
  dirtyBar: 1001,
  modal: 1010,
  /** The image lightbox, over modals. */
  lightbox: 9999,
  /** Fixed-position popups (HoverHint, autocomplete, the cost menu): over everything. */
  overlay: 99999,
} as const;
export const Z_DIRTY_BAR = Z.dirtyBar;
const COMPONENTS = { Modal: { zIndexPopupBase: Z.modal } };

/**
 * AntD's tokens. Every value that ALSO exists in the palette (ThemeContext.tsx)
 * is read from it, never copied: a copy drifts. AntD's secondary text is the
 * palette's TEXT_MUTED and its placeholder TEXT_FAINT in both themes. The
 * literals left are AntD-only: link active, and the light status fills (the
 * light ACCENT_* are text-grade, darker than a fill needs).
 */
const DG_BRAND = {
  fontFamily: FONT_UI,
  fontFamilyCode: FONT_MONO,
};

export const darkTheme = {
  algorithm: theme.darkAlgorithm,
  token: {
    ...DG_BRAND,
    colorPrimary: D.ACCENT_BLUE,
    colorBgBase: D.BG_BASE,
    colorBgContainer: D.BG_CARD,
    colorBgElevated: D.BG_ELEVATED,
    colorBorderSecondary: D.BORDER,
    colorSuccess: D.ACCENT_GREEN,
    colorError: D.ACCENT_RED,
    colorWarning: D.ACCENT_AMBER,
    colorTextBase: D.TEXT_PRIMARY,
    // WCAG AA (4.5:1): text on the teal primary is dark, not white (2.5:1),
    // and the derived secondary/placeholder greys are lifted.
    colorTextLightSolid: D.ON_ACCENT,
    colorTextSecondary: D.TEXT_MUTED,
    colorTextTertiary: D.TEXT_MUTED,
    colorTextPlaceholder: D.TEXT_FAINT,
    // Links use the brand teal, not AntD's default blue.
    colorLink: D.ACCENT_BLUE,
    colorLinkHover: D.ACCENT_BLUE_LIGHT,
    colorLinkActive: '#14b8a6',
    // The dark algorithm derives #1f6a61 from the teal primary: the text of a
    // hovered selected option read 4.27:1 there (contrast.test.ts).
    controlItemBgActiveHover: '#1e5d55',
    fontSize: 14,
    borderRadius: 8,
  },
  components: COMPONENTS,
};

export const lightTheme = {
  algorithm: theme.defaultAlgorithm,
  token: {
    ...DG_BRAND,
    // #0d9488 under white button text is 3.7:1; #0f766e is 5.5:1 (WCAG AA).
    colorPrimary: L.ACCENT_BLUE,
    colorTextLightSolid: L.ON_ACCENT,
    colorLink: L.ACCENT_BLUE,
    colorLinkHover: L.ACCENT_BLUE_LIGHT,
    colorLinkActive: '#134e4a',
    colorTextSecondary: L.TEXT_MUTED,
    colorTextTertiary: L.TEXT_MUTED,
    colorTextPlaceholder: L.TEXT_FAINT,
    colorBgBase: L.BG_BASE,
    colorBgContainer: L.BG_CARD,
    colorBgElevated: L.BG_ELEVATED,
    colorBorderSecondary: L.BORDER,
    colorSuccess: '#059669',
    colorError: '#e11d48',
    colorWarning: '#d97706',
    colorTextBase: L.TEXT_PRIMARY,
    borderRadius: 8,
  },
  components: COMPONENTS,
};

/**
 * Pure: AntD's theme for a style and mode. The Instrument style is
 * darkTheme / lightTheme above; Editorial reads its palette the same way and
 * adds its shapes: pill buttons, 12 px fields, 20 px cards and 24 px modals.
 */
export function antdTheme(style: ThemeStyle, isDark: boolean) {
  if (style === 'instrument') return isDark ? darkTheme : lightTheme;
  const c = PALETTES[style][isDark ? 'dark' : 'light'];
  return {
    algorithm: isDark ? theme.darkAlgorithm : theme.defaultAlgorithm,
    token: {
      ...DG_BRAND,
      colorPrimary: c.ACCENT_BLUE,
      // The algorithm's derived hover (light) and press (dark) fills sit
      // under 4.5:1 with ON_ACCENT; the palette's companion keeps it.
      colorPrimaryHover: c.ACCENT_BLUE_LIGHT,
      colorPrimaryActive: c.ACCENT_BLUE,
      colorLink: c.ACCENT_BLUE,
      colorLinkHover: c.ACCENT_BLUE_LIGHT,
      colorLinkActive: c.ACCENT_BLUE_LIGHT,
      colorTextLightSolid: c.ON_ACCENT,
      colorBgBase: c.BG_BASE,
      colorBgContainer: c.BG_CARD,
      colorBgElevated: c.BG_ELEVATED,
      colorBgLayout: c.BG_BASE,
      colorBorder: c.BORDER,
      colorBorderSecondary: c.BORDER,
      colorTextBase: c.TEXT_PRIMARY,
      colorTextSecondary: c.TEXT_MUTED,
      colorTextTertiary: c.TEXT_MUTED,
      colorTextPlaceholder: c.TEXT_FAINT,
      // Status SEEDS: AntD derives the tag tints from them, and the deep
      // text-grade accents of the light palette give muddy tints. By day the
      // seeds are fill-grade; by night the accents are light enough.
      ...(isDark
        ? { colorSuccess: c.ACCENT_GREEN, colorError: c.ACCENT_RED, colorWarning: c.ACCENT_AMBER }
        : { colorSuccess: '#2f8f6b', colorError: '#c22a3e', colorWarning: '#e0a100' }),
      // Info is calm ink on paper, not AntD's blue (the style has no blue).
      // The tints are set, not derived: AntD derives muddy ones from a grey seed.
      colorInfo: c.TEXT_SECONDARY,
      colorInfoBg: c.BG_SIDEBAR,
      colorInfoBorder: c.BORDER,
      colorInfoText: c.TEXT_PRIMARY,
      fontSize: 14,
      borderRadius: 12,
      borderRadiusLG: 20,
      borderRadiusSM: 8,
    },
    components: {
      ...COMPONENTS,
      Button: { borderRadius: 999, borderRadiusLG: 999, borderRadiusSM: 999, fontWeight: 600 },
      Modal: { ...COMPONENTS.Modal, borderRadiusLG: 24 },
    },
  };
}
