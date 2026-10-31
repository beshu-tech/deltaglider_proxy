/**
 * Mermaid diagrams in the brand (UX harvest D48): the default mermaid
 * palette (yellow boxes, periwinkle nodes) and 9 px text are replaced by
 * the theme tokens and a text size that never drops below 12 px.
 */
import type { ColorTokens } from './ThemeContext';

/** The diagram text size, and the smallest size it may be scaled down to. */
const MERMAID_FONT_PX = 14;
const MIN_TEXT_PX = 12;

/** Pure: a hex colour `t` of the way from `a` to `b` (both #rrggbb). */
export function mixHex(a: string, b: string, t: number): string {
  const ch = (hex: string, i: number) => parseInt(hex.slice(1 + i * 2, 3 + i * 2), 16);
  const out = [0, 1, 2].map((i) => Math.round(ch(a, i) + (ch(b, i) - ch(a, i)) * t));
  return `#${out.map((v) => v.toString(16).padStart(2, '0')).join('')}`;
}

/** Pure: the mermaid `initialize` options for the active theme. */
export function mermaidConfig(colors: ColorTokens, isDark: boolean) {
  const node = mixHex(colors.BG_CARD, colors.ACCENT_BLUE, isDark ? 0.18 : 0.1);
  return {
    startOnLoad: false,
    securityLevel: 'strict' as const,
    // 'base' is the one mermaid theme that takes every colour from themeVariables.
    theme: 'base' as const,
    themeVariables: {
      darkMode: isDark,
      fontFamily: "'Manrope', -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif",
      fontSize: `${MERMAID_FONT_PX}px`,
      background: colors.BG_CARD,
      primaryColor: node,
      primaryBorderColor: colors.ACCENT_BLUE,
      primaryTextColor: colors.TEXT_PRIMARY,
      secondaryColor: colors.BG_ELEVATED,
      secondaryBorderColor: colors.BORDER,
      secondaryTextColor: colors.TEXT_PRIMARY,
      tertiaryColor: colors.BG_CARD,
      tertiaryBorderColor: colors.BORDER,
      tertiaryTextColor: colors.TEXT_PRIMARY,
      mainBkg: node,
      nodeBorder: colors.ACCENT_BLUE,
      nodeTextColor: colors.TEXT_PRIMARY,
      textColor: colors.TEXT_PRIMARY,
      lineColor: colors.TEXT_MUTED,
      clusterBkg: colors.BG_BASE,
      clusterBorder: colors.BORDER,
      titleColor: colors.TEXT_PRIMARY,
      edgeLabelBackground: colors.BG_CARD,
      noteBkgColor: colors.BG_ELEVATED,
      noteBorderColor: colors.BORDER,
      noteTextColor: colors.TEXT_PRIMARY,
      actorBkg: node,
      actorBorder: colors.ACCENT_BLUE,
      actorTextColor: colors.TEXT_PRIMARY,
      signalColor: colors.TEXT_MUTED,
      signalTextColor: colors.TEXT_PRIMARY,
      labelBoxBkgColor: colors.BG_ELEVATED,
      labelTextColor: colors.TEXT_PRIMARY,
    },
    flowchart: { useMaxWidth: false },
    sequence: { useMaxWidth: false },
  };
}

/**
 * Pure: the width (px) to draw a diagram whose natural width is
 * `naturalW` in a container `containerW` wide. It fits the container, is
 * never scaled up, and is never scaled down so far that the text is
 * smaller than 12 px: the container then scrolls sideways instead.
 */
export function mermaidDisplayWidth(naturalW: number, containerW: number): number {
  const fit = Math.min(naturalW, containerW);
  const minReadable = naturalW * (MIN_TEXT_PX / MERMAID_FONT_PX);
  return Math.ceil(Math.max(fit, minReadable));
}
