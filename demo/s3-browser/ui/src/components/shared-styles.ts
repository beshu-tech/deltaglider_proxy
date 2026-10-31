import { useMemo } from 'react';
import { useColors } from '../ThemeContext';

/**
 * LAYOUT TOKENS — one grid, one rhythm (UX harvest D22).
 *
 * Every margin, gap and padding in an admin panel is a value of SPACE. The
 * source test `layoutTokens.test.ts` refuses other numeric literals for
 * margin / gap / padding in admin panels (files not yet migrated are on its
 * allow-list).
 *
 *   SPACE.xxs 4   icon ↔ text, chip inner gap
 *   SPACE.xs  8   controls in one row, label ↔ field
 *   SPACE.sm  12  rows of a list, fields in a group
 *   SPACE.md  16  fields in a form, a card's inner groups
 *   SPACE.lg  24  between cards (CARD_GAP), card padding (CARD_PADDING)
 *   SPACE.xl  32  between sections of a page
 *
 * One card shape: radius CARD_RADIUS (12), padding CARD_PADDING, CARD_GAP
 * below it. Use `useCardStyles().cardStyle` rather than a hand-made card.
 */
export const SPACE = { xxs: 4, xs: 8, sm: 12, md: 16, lg: 24, xl: 32 } as const;
/** The allowed values, for the source test (0 is always allowed). */
export const SPACING_SCALE: readonly number[] = [0, ...Object.values(SPACE)];
export const CARD_RADIUS = 12;

/**
 * TYPE SCALE — the font sizes of the app (UX audit P3). AntD's body is 14.
 *
 *   TYPE.xs   11  eyebrows, chips, captions: the floor for text
 *   TYPE.sm   12  secondary text, help, table meta
 *   TYPE.md   13  dense body text (lists, cards)
 *   TYPE.base 14  body text, AntD controls
 *   TYPE.lg   16  card titles
 *   TYPE.xl   20  section headings
 *   TYPE.xxl  24  page headings
 *   TYPE.hero 28  a stat figure
 * DISPLAY holds the analytics hero numerals (sized by heroNumberSize.ts).
 * `typeScale.test.ts` refuses other sizes (files not yet migrated are on its
 * allow-list); an icon's size is not text and is not checked.
 */
export const TYPE = { xs: 11, sm: 12, md: 13, base: 14, lg: 16, xl: 20, xxl: 24, hero: 28 } as const;
export const DISPLAY_SIZES: readonly number[] = [32, 36, 40, 48, 56];
/** The allowed values, for the source test. */
export const TYPE_SCALE: readonly number[] = [...Object.values(TYPE), ...DISPLAY_SIZES];
/** 24 px on a desktop, down to 16 px on a phone. */
export const CARD_PADDING = `clamp(${SPACE.md}px, 3vw, ${SPACE.lg}px)`;
export const CARD_GAP = SPACE.lg;

/**
 * Two shared content-column widths for admin section panels — replaces the
 * per-panel magic numbers (740/820/860/960/1100/1180). FORM suits single-column
 * forms and list+detail layouts (readable line length); WIDE suits data tables.
 * Spread `contentColumn(FORM|WIDE)` onto a panel's outer wrapper.
 */
export const CONTENT_FORM = 880;

/**
 * VIEWPORT BREAKPOINTS — `useIsNarrow(BREAKPOINT.x)` is true below x px. A
 * stylesheet writes `max-width: <x - 1>px` for the same edge (source test
 * breakpoints.test.ts). A component with a reason of its own names a
 * constant (ICON_ONLY_BELOW, …) instead.
 *
 *   phone  600  one column, icon buttons
 *   tablet 769  the docs sidebar hides; theme.css grids collapse
 *   shell  900  the admin sidebar becomes a drawer
 */
export const BREAKPOINT = { phone: 600, tablet: 769, shell: 900 } as const;
export const CONTENT_WIDE = 1140;

export function contentColumn(width: number): React.CSSProperties {
  return { maxWidth: width, margin: '0 auto', padding: CARD_PADDING };
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
      borderRadius: CARD_RADIUS,
      padding: CARD_PADDING,
      marginBottom: CARD_GAP,
    };
    const labelStyle: React.CSSProperties = {
      color: TEXT_MUTED,
      fontSize: 12,
      fontWeight: 600,
      marginBottom: 6,
      display: 'block',
      fontFamily: 'var(--font-ui)',
    };
    const inputRadius = { borderRadius: 8 };
    return { cardStyle, labelStyle, inputRadius };
  }, [BG_CARD, BORDER, TEXT_MUTED]);
}

/**
 * Form-label style object (sentence case) (spread onto a wrapper div) for panels that
 * render labels as plain divs rather than the <FormLabel> component — e.g.
 * AuthenticationPanel's provider/preview fields. Memoized for stable identity.
 */
export function useFormLabelStyle(): React.CSSProperties {
  const { TEXT_MUTED } = useColors();
  return useMemo(
    () => ({
      fontSize: 12,
      fontWeight: 600,
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
      fontSize: 12,
      fontWeight: 600,
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

