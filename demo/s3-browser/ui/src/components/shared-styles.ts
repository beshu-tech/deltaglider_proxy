import { useColors } from '../ThemeContext';

/** Shared card, label, and input styles used across Settings sub-components. */
export function useCardStyles() {
  const { BG_CARD, BORDER, TEXT_MUTED } = useColors();
  const cardStyle: React.CSSProperties = {
    background: BG_CARD,
    border: `1px solid ${BORDER}`,
    borderRadius: 12,
    padding: 'clamp(16px, 3vw, 24px)',
    marginBottom: 16,
  };
  const labelStyle: React.CSSProperties = {
    color: TEXT_MUTED,
    fontSize: 11,
    fontWeight: 600,
    letterSpacing: 0.5,
    textTransform: 'uppercase' as const,
    marginBottom: 6,
    display: 'block',
    fontFamily: "var(--font-ui)",
  };
  const inputRadius = { borderRadius: 8 };
  return { cardStyle, labelStyle, inputRadius };
}

/** Glass-card container style for full-page forms (login, connect). */
export const glassCardFormStyle: React.CSSProperties = {
  borderRadius: 14,
  padding: 'clamp(28px, 4vw, 40px)',
  width: '100%',
  maxWidth: 440,
};

/** Standard large input style. */
export function useLargeInputStyle() {
  const { BORDER } = useColors();
  return {
    background: 'var(--input-bg)',
    borderColor: BORDER,
    borderRadius: 10,
    height: 44,
    fontFamily: "var(--font-mono)" as const,
    fontSize: 13,
  };
}
