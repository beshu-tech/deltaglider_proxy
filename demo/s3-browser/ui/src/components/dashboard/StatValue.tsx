/**
 * StatValue — the single-big-number body used inside a Panel.
 *
 * Use when the panel's main story is one number (Total Requests,
 * Peak Memory, Savings %). Optional children slot under the number
 * for progress bars / sparklines / secondary facts.
 *
 * Typography:
 *   - Value: clamp between 28px and 44px based on viewport width.
 *     `font-variant-numeric: tabular-nums` so digits don't shift
 *     width as values tick.
 *   - Unit: smaller, muted, rendered inline.
 *   - Hint: one-liner under the number, TEXT_MUTED.
 *
 * Colour the value by passing `tone='good' | 'warn' | 'bad'`. Uses
 * accent tokens from the theme so it stays on-brand in light and
 * dark.
 */
import type { ReactNode } from 'react';
import { useColors } from '../../ThemeContext';

type StatTone = 'neutral' | 'good' | 'warn' | 'bad';

interface Props {
  value: string;
  /** Optional trailing unit in smaller type (e.g. "%", "MB"). */
  unit?: string;
  /** One-line secondary caption shown under the value. */
  hint?: string;
  /** Semantic colouring for the number itself. */
  tone?: StatTone;
  /** Anything under the hint — progress bar, sparkline, etc. */
  children?: ReactNode;
}

export default function StatValue({ value, unit, hint, tone = 'neutral', children }: Props) {
  const colors = useColors();
  const color = (() => {
    switch (tone) {
      case 'good':
        return colors.ACCENT_GREEN;
      case 'warn':
        return colors.ACCENT_AMBER;
      case 'bad':
        return colors.ACCENT_RED;
      default:
        return colors.TEXT_PRIMARY;
    }
  })();

  return (
    <div
      style={{
        display: 'flex',
        flexDirection: 'column',
        justifyContent: 'center',
        flex: 1,
        gap: 4,
      }}
    >
      <div
        style={{
          display: 'flex',
          alignItems: 'baseline',
          gap: 6,
          color,
          fontFamily: 'var(--font-ui)',
          fontWeight: 700,
          // Fluid typography — below 1280 → ~28px; above 2400 → 44px.
          // `clamp(28px, 1.6vw + 10px, 44px)` lands in the sweet spot
          // for our layout (panels average 220–380px wide).
          fontSize: 'clamp(28px, 1.6vw + 10px, 44px)',
          lineHeight: 1.1,
          letterSpacing: '-0.02em',
          fontVariantNumeric: 'tabular-nums',
        }}
      >
        <span>{value}</span>
        {unit && (
          <span
            style={{
              fontSize: 'clamp(14px, 0.5vw + 10px, 18px)',
              fontWeight: 500,
              color: colors.TEXT_MUTED,
              letterSpacing: 0,
            }}
          >
            {unit}
          </span>
        )}
      </div>
      {hint && (
        <div
          style={{
            fontSize: 11.5,
            color: colors.TEXT_MUTED,
            fontFamily: 'var(--font-ui)',
            lineHeight: 1.4,
            // Allow 2 lines before ellipsis — some hints carry
            // multi-fact context ("45.6 GB original data (sampled)").
            display: '-webkit-box',
            WebkitLineClamp: 2,
            WebkitBoxOrient: 'vertical',
            overflow: 'hidden',
          }}
        >
          {hint}
        </div>
      )}
      {children}
    </div>
  );
}
