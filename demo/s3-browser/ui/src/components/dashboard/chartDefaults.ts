/**
 * Shared Recharts defaults for the redesigned dashboard.
 *
 * One import, one palette. Every panel uses these so multi-series
 * charts cycle through the same hues in the same order — the reader
 * can tell "teal = primary operation" without a legend on each card.
 */

import type { ColorTokens } from '../../ThemeContext';

/**
 * Ordered palette for multi-series panels. Start from ACCENT_BLUE
 * (teal, the product's primary) and fan out across a harmonic set
 * tuned to work on both dark and light backgrounds. Exported as an
 * array so callers can slice + cycle by index.
 */
export const CHART_PALETTE = [
  '#2dd4bf', // teal   — primary, "default series"
  '#60a5fa', // blue   — secondary
  '#a78bfa', // purple — tertiary
  '#fbbf24', // amber  — warning / ratio
  '#fb7185', // rose   — error / deny
  '#34d399', // green  — success / allow
  '#f472b6', // pink   — spill for 7+ series
  '#818cf8', // indigo — spill for 8+ series
] as const;

/** Status-code colour map — used on HTTP panels. */
export const STATUS_COLORS: Record<string, string> = {
  '2xx': '#2dd4bf',
  '3xx': '#60a5fa',
  '4xx': '#fbbf24',
  '5xx': '#fb7185',
};

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
    fontSize: 10,
    fill: colors.TEXT_MUTED,
    fontFamily: mono ? 'var(--font-mono)' : 'var(--font-ui)',
  };
}

// ── Formatters ──────────────────────────────────────────────────

export function fmtDuration(s: number): string {
  if (!isFinite(s) || s <= 0) return '—';
  if (s < 0.001) return `${(s * 1e6).toFixed(0)}µs`;
  if (s < 1) return `${(s * 1000).toFixed(1)}ms`;
  return `${s.toFixed(2)}s`;
}

export function fmtPct(ratio: number): string {
  if (!isFinite(ratio)) return '—';
  return `${(ratio * 100).toFixed(1)}%`;
}

export function fmtNum(n: number): string {
  if (!isFinite(n)) return '—';
  return n.toLocaleString(undefined, { maximumFractionDigits: 0 });
}
