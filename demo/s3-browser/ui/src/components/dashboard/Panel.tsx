/**
 * Panel — one tile on the DashboardGrid.
 *
 * A Panel is a self-contained visualisation with:
 *   - fixed-height header (title + subtitle + actions)
 *   - fluid body that fills available space
 *   - optional accent top border (for category signalling)
 *   - loading / empty / error states
 *
 * The `colSpan` + `rowSpan` props drive the grid placement. Each row
 * of a dashboard sums to 12; below 900px every panel takes the full
 * width (dashboard.css).
 */
import { useEffect, useRef } from 'react';
import type { ReactNode } from 'react';
import { EmptyState, SkeletonState } from '../StatePlaceholders';
import { useColors } from '../../ThemeContext';
import { CARD_RADIUS, SPACE } from '../shared-styles';
import type { ColorTokens } from '../../ThemeContext';

type PanelSpan = 3 | 4 | 6 | 8 | 12;
type PanelRows = 1 | 2 | 3;
type PanelAccent = 'blue' | 'green' | 'red' | 'amber' | 'purple';

interface Props {
  /** Panel heading. Short. */
  title: string;
  /** One-line muted description under the title. Keep concise. */
  subtitle?: string;
  /** Column span of 12. The spans of one row sum to 12. */
  colSpan: PanelSpan;
  /** Row span at comfortable density. Default 1. */
  rowSpan?: PanelRows;
  /** Right-side actions slot — tiny buttons or tag-like chips. */
  actions?: ReactNode;
  /** Status stripe along the top border. Set it only for a state that needs
   *  attention (bad / warn), never for "all good": a stripe on a healthy tile
   *  reads as "selected" or "alert". */
  accent?: PanelAccent;
  /** Shows a skeleton in place of the body, never placeholder figures. */
  loading?: boolean;
  /** When truthy, body is replaced with an empty-state message. */
  empty?: { title: string; hint?: string };
  /** Panel body. Can be a chart (will fill 100% of remaining space)
   *  or arbitrary content. */
  children: ReactNode;
}

export default function Panel({
  title,
  subtitle,
  colSpan,
  rowSpan = 1,
  actions,
  accent,
  loading,
  empty,
  children,
}: Props) {
  const colors = useColors();

  const accentHex = accent ? accentColor(colors, accent) : null;
  // Light theme gets a machined top inner highlight; dark a faint one.
  const isLight = colors.BG_CARD === '#ffffff';
  return (
    <div
      data-colspan={colSpan}
      data-rowspan={rowSpan}
      data-accent={accent || ''}
      style={{
        // Comfortable density default.
        gridColumn: `span ${colSpan}`,
        gridRow: `span ${rowSpan}`,
        // One card chrome for every tile: the accent is only the top stripe.
        background: colors.BG_CARD,
        border: `1px solid ${colors.BORDER}`,
        borderRadius: CARD_RADIUS,
        boxShadow: `${colors.ELEV_SHADOW}, inset 0 1px 0 ${isLight ? 'rgba(255,255,255,0.7)' : 'rgba(255,255,255,0.04)'}`,
        padding: 0,
        display: 'flex',
        flexDirection: 'column',
        minWidth: 0,
        minHeight: 0,
        overflow: 'hidden',
        boxSizing: 'border-box',
        position: 'relative',
      }}
    >
      {/* Accent edge: a fading gradient blade instead of a blunt 2px border. */}
      {accentHex && (
        <div
          aria-hidden
          style={{
            position: 'absolute',
            top: 0,
            left: SPACE.md,
            right: SPACE.md,
            height: 2,
            borderRadius: 2,
            background: `linear-gradient(90deg, transparent, ${accentHex} 18%, ${accentHex} 82%, transparent)`,
            opacity: 0.9,
            zIndex: 1,
          }}
        />
      )}
      <PanelHeader title={title} subtitle={subtitle} actions={actions} colors={colors} />
      <PanelBody loading={loading} empty={empty}>
        {children}
      </PanelBody>
    </div>
  );
}

function PanelHeader({
  title,
  subtitle,
  actions,
  colors,
}: {
  title: string;
  subtitle?: string;
  actions?: ReactNode;
  colors: ColorTokens;
}) {
  return (
    <div
      style={{
        display: 'flex',
        alignItems: 'flex-start',
        justifyContent: 'space-between',
        // Wrap so the actions toolbar drops below the title instead of
        // overflowing on narrow (Re-scan-all was clipped on mobile).
        flexWrap: 'wrap',
        gap: SPACE.sm,
        rowGap: SPACE.xs,
        padding: `${SPACE.sm}px ${SPACE.md}px`,
        borderBottom: `1px solid ${colors.BORDER}`,
        background: 'transparent',
        flexShrink: 0,
      }}
    >
      {/* flex-basis keeps the title on the row until there's no room. */}
      <div style={{ minWidth: 0, flex: '1 1 180px' }}>
        <div
          style={{
            fontSize: 13,
            fontWeight: 700,
            color: colors.TEXT_PRIMARY,
            fontFamily: 'var(--font-ui)',
            letterSpacing: '-0.01em',
            whiteSpace: 'nowrap',
            overflow: 'hidden',
            textOverflow: 'ellipsis',
            lineHeight: 1.35,
          }}
        >
          {title}
        </div>
        {subtitle && (
          <div
            style={{
              fontSize: 11,
              color: colors.TEXT_MUTED,
              fontFamily: 'var(--font-ui)',
              lineHeight: 1.4,
              marginTop: 0,
              whiteSpace: 'nowrap',
              overflow: 'hidden',
              textOverflow: 'ellipsis',
            }}
          >
            {subtitle}
          </div>
        )}
      </div>
      {actions && (
        // `flexShrink: 0` sized the toolbar to its content, so on a phone it
        // ran past the card edge instead of wrapping its own buttons.
        <div style={{ display: 'flex', alignItems: 'center', gap: SPACE.xs, flexWrap: 'wrap', flex: '0 1 auto', minWidth: 0, maxWidth: '100%' }}>
          {actions}
        </div>
      )}
    </div>
  );
}

function PanelBody({
  loading,
  empty,
  children,
}: {
  loading?: boolean;
  empty?: { title: string; hint?: string };
  children: ReactNode;
}) {
  // ResponsiveContainer from Recharts requires the body to have a
  // well-defined height; flex:1 + minHeight:0 inside the column
  // flex of the outer Panel gives it one. We also set
  // position:relative so absolutely-positioned overlays (empty
  // state, loading) can stack without taking the chart out of flow.
  const bodyRef = useRef<HTMLDivElement>(null);
  // Some charts misreport dimensions on the first tick if the body
  // was display:none during layout. Nudge them once we're mounted.
  useEffect(() => {
    if (!bodyRef.current) return;
    // Force a layout read — Recharts picks up the real size on
    // its internal ResizeObserver tick after this.
    void bodyRef.current.offsetHeight;
  }, []);

  if (loading) {
    return <SkeletonState />;
  }

  if (empty) {
    return <EmptyState compact title={empty.title} hint={empty.hint} />;
  }

  return (
    <div
      ref={bodyRef}
      style={{
        flex: 1,
        minHeight: 0,
        position: 'relative',
        padding: SPACE.md,
        display: 'flex',
        flexDirection: 'column',
        gap: SPACE.xs,
      }}
    >
      {children}
    </div>
  );
}

function accentColor(colors: ColorTokens, accent: PanelAccent): string {
  switch (accent) {
    case 'blue':
      return colors.ACCENT_BLUE;
    case 'green':
      return colors.ACCENT_GREEN;
    case 'red':
      return colors.ACCENT_RED;
    case 'amber':
      return colors.ACCENT_AMBER;
    case 'purple':
      return colors.ACCENT_PURPLE;
  }
}
