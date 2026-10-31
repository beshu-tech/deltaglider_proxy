/**
 * Shared Recharts defaults for the redesigned dashboard.
 *
 * One import, one palette. Every panel uses these so multi-series
 * charts cycle through the same hues in the same order — the reader
 * can tell "teal = primary operation" without a legend on each card.
 */

import type { ColorTokens } from '../../ThemeContext';

/**
 * THE chart colours (UX harvest D21): every series comes from the theme
 * tokens, so a chart matches the rest of the admin in light AND dark.
 * Teal (the brand primary) first, amber second; then slate, green, rose,
 * violet. No AntD default blue: it was the only blue element on the page.
 * A single-series chart uses `primary` for every bar; `series` is for
 * charts whose bars are different things (one colour per category).
 */
export function chartPalette(colors: ColorTokens): { primary: string; secondary: string; series: string[] } {
  const series = [
    colors.ACCENT_BLUE, // teal — the primary series
    colors.ACCENT_AMBER, // amber — the secondary series
    colors.TEXT_MUTED, // slate
    colors.ACCENT_GREEN,
    colors.ACCENT_RED,
    colors.ACCENT_PURPLE,
  ];
  return { primary: series[0], secondary: series[1], series };
}

/** HTTP status class → colour: success teal, redirect slate, client error amber, server error rose. */
export function statusColors(colors: ColorTokens): Record<string, string> {
  return {
    '2xx': colors.ACCENT_BLUE,
    '3xx': colors.TEXT_MUTED,
    '4xx': colors.ACCENT_AMBER,
    '5xx': colors.ACCENT_RED,
  };
}

/**
 * Build the shared tooltip style from the active theme. Recharts
 * `<Tooltip {...obj}>` applies `contentStyle`, `labelStyle`,
 * `itemStyle` straight through, so this replaces the inline object
 * that was copy-pasted in five places.
 */
export function chartTooltipStyle(colors: ColorTokens) {
  return {
    contentStyle: {
      background: colors.BG_CARD,
      border: `1px solid ${colors.BORDER}`,
      borderRadius: 8,
      fontSize: 12,
      fontFamily: 'var(--font-ui)',
      color: colors.TEXT_PRIMARY,
      padding: '8px 12px',
    },
    labelStyle: {
      color: colors.TEXT_MUTED,
      fontSize: 11,
      marginBottom: 4,
    },
    itemStyle: {
      color: colors.TEXT_SECONDARY,
      padding: '2px 0',
    },
    cursor: {
      stroke: colors.ACCENT_BLUE,
      strokeDasharray: '3 3',
      strokeOpacity: 0.5,
    },
  };
}

/** Tick style for axis labels. Small, muted, mono for numbers. */
export function axisTickStyle(colors: ColorTokens, mono = false) {
  return {
    fontSize: 11,
    fill: colors.TEXT_MUTED,
    fontFamily: mono ? 'var(--font-mono)' : 'var(--font-ui)',
  };
}

// ── Formatters ──────────────────────────────────────────────────

export function fmtDuration(s: number): string {
  if (!isFinite(s) || s <= 0) return '—';
  if (s < 0.001) return `${(s * 1e6).toFixed(0)} µs`;
  if (s < 1) return `${(s * 1000).toFixed(1)} ms`;
  return `${s.toFixed(2)} s`;
}

export function fmtPct(ratio: number): string {
  if (!isFinite(ratio)) return '—';
  return `${(ratio * 100).toFixed(1)}%`;
}

export function fmtNum(n: number): string {
  if (!isFinite(n)) return '—';
  return n.toLocaleString(undefined, { maximumFractionDigits: 0 });
}
