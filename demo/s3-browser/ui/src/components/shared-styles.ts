import { useMemo } from 'react';
import { useColors } from '../ThemeContext';

/**
 * Shared card, label, and input styles used across Settings
 * sub-components.
 *
 * Memoized against the three theme colors read — so the returned
 * object identity is stable across renders as long as the theme
 * doesn't change. Previously this hook allocated three fresh
 * style objects on every call; consumers like CredentialsModePanel
 * / AdmissionPanel / BucketsPanel / advancedPanels pass these into
 * JSX children every render, silently breaking any future
 * `React.memo` wrap and making the "is the style object stable"
 * story inconsistent with the rest of the code base.
 */
export function useCardStyles() {
  const { BG_CARD, BORDER, TEXT_MUTED } = useColors();
  return useMemo(() => {
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
      fontFamily: 'var(--font-ui)',
    };
    const inputRadius = { borderRadius: 8 };
    return { cardStyle, labelStyle, inputRadius };
  }, [BG_CARD, BORDER, TEXT_MUTED]);
}

