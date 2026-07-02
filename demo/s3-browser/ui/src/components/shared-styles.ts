import { useMemo } from 'react';
import { useColors } from '../ThemeContext';

/**
 * Two shared content-column widths for admin section panels — replaces the
 * per-panel magic numbers (740/820/860/960/1100/1180). FORM suits single-column
 * forms and list+detail layouts (readable line length); WIDE suits data tables.
 * Spread `contentColumn(FORM|WIDE)` onto a panel's outer wrapper.
 */
export const CONTENT_FORM = 880;
export const CONTENT_WIDE = 1140;

export function contentColumn(width: number): React.CSSProperties {
  return { maxWidth: width, margin: '0 auto', padding: 'clamp(16px, 3vw, 24px)' };
}

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

/**
 * Uppercase form-label style object (spread onto a wrapper div) for panels that
 * render labels as plain divs rather than the <FormLabel> component — e.g.
 * AuthenticationPanel's provider/preview fields. Memoized for stable identity.
 */
export function useFormLabelStyle(): React.CSSProperties {
  const { TEXT_MUTED } = useColors();
  return useMemo(
    () => ({
      fontSize: 11,
      fontWeight: 600,
      textTransform: 'uppercase' as const,
      letterSpacing: 0.5,
      color: TEXT_MUTED,
      fontFamily: 'var(--font-ui)',
      marginBottom: 4,
    }),
    [TEXT_MUTED],
  );
}

/**
 * Theme-aware style objects shared by PermissionEditor (condition labels +
 * inline mono spans). Memoized so the object identity is stable across renders
 * while the theme is unchanged.
 */
export function usePermissionStyles() {
  const { TEXT_MUTED, TEXT_PRIMARY } = useColors();
  return useMemo(() => {
    const condLabelStyle: React.CSSProperties = {
      fontSize: 10,
      fontWeight: 600,
      letterSpacing: 0.5,
      textTransform: 'uppercase',
      color: TEXT_MUTED,
      fontFamily: 'var(--font-ui)',
    };
    const monoTextStyle: React.CSSProperties = {
      fontFamily: 'var(--font-mono)',
      color: TEXT_PRIMARY,
    };
    return { condLabelStyle, monoTextStyle };
  }, [TEXT_MUTED, TEXT_PRIMARY]);
}

