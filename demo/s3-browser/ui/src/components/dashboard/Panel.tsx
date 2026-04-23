/**
 * Panel — one tile on the DashboardGrid.
 *
 * A Panel is a self-contained visualisation with:
 *   - fixed-height header (title + subtitle + actions)
 *   - fluid body that fills available space
 *   - optional accent top border (for category signalling)
 *   - loading / empty / error states
 *
 * The `colSpan` + `rowSpan` props drive the grid placement. Panels
 * declare their *comfortable* span; below 1280px the parent grid
 * collapses spans to half (capped at 6) so the same row descriptions
 * still look right on a 1080p laptop.
 */
import { useEffect, useRef } from 'react';
import type { ReactNode } from 'react';
import { useColors } from '../../ThemeContext';
import type { ColorTokens } from '../../ThemeContext';

type PanelSpan = 3 | 4 | 6 | 8 | 12;
type PanelRows = 1 | 2 | 3;
type PanelAccent = 'blue' | 'green' | 'red' | 'amber' | 'purple';

interface Props {
  /** Panel heading. Short. */
  title: string;
  /** One-line muted description under the title. Keep concise. */
  subtitle?: string;
  /** Column span at comfortable density (1280px+). Collapses to half below. */
  colSpan: PanelSpan;
  /** Row span at comfortable density. Default 1. */
  rowSpan?: PanelRows;
  /** Right-side actions slot — tiny buttons or tag-like chips. */
  actions?: ReactNode;
  /** Accent stripe along the top border. */
  accent?: PanelAccent;
  /** Shows a spinner/skeleton in place of body. */
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

  // Grid placement. At comfortable density every panel gets its
  // stated colSpan; at compact density the grid reads the `data-
  // density` attribute from the parent DashboardGrid (set via a
  // ResizeObserver) and the CSS below collapses spans to half.
  // We encode the intended span on a data attribute so the CSS
  // rule below can match on it — inline styles can't do media-free
  // conditional logic.
  return (
    <div
      data-colspan={colSpan}
      data-rowspan={rowSpan}
      data-accent={accent || ''}
      style={{
        // Comfortable density default.
        gridColumn: `span ${colSpan}`,
        gridRow: `span ${rowSpan}`,
        background: colors.BG_CARD,
        border: `1px solid ${colors.BORDER}`,
        borderTop: accent
          ? `2px solid ${accentColor(colors, accent)}`
          : `1px solid ${colors.BORDER}`,
        borderRadius: 10,
        padding: 0,
        display: 'flex',
        flexDirection: 'column',
        minWidth: 0,
        minHeight: 0,
        overflow: 'hidden',
        boxSizing: 'border-box',
      }}
    >
      <PanelHeader title={title} subtitle={subtitle} actions={actions} colors={colors} />
      <PanelBody loading={loading} empty={empty} colors={colors}>
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
        gap: 12,
        padding: '10px 14px',
        borderBottom: `1px solid ${colors.BORDER}`,
        background: 'transparent',
        flexShrink: 0,
      }}
    >
      <div style={{ minWidth: 0, flex: 1 }}>
        <div
          style={{
            fontSize: 13,
            fontWeight: 600,
            color: colors.TEXT_PRIMARY,
            fontFamily: 'var(--font-ui)',
            letterSpacing: '-0.005em',
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
              marginTop: 1,
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
        <div style={{ display: 'flex', alignItems: 'center', gap: 6, flexShrink: 0 }}>
          {actions}
        </div>
      )}
    </div>
  );
}

function PanelBody({
  loading,
  empty,
  colors,
  children,
}: {
  loading?: boolean;
  empty?: { title: string; hint?: string };
  colors: ColorTokens;
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
    return (
      <div
        style={{
          flex: 1,
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'center',
          padding: 16,
          color: colors.TEXT_MUTED,
          fontSize: 12,
        }}
      >
        Loading…
      </div>
    );
  }

  if (empty) {
    return (
      <div
        style={{
          flex: 1,
          display: 'flex',
          flexDirection: 'column',
          alignItems: 'center',
          justifyContent: 'center',
          gap: 4,
          padding: 16,
          textAlign: 'center',
        }}
      >
        <div
          style={{
            fontSize: 13,
            color: colors.TEXT_SECONDARY,
            fontWeight: 500,
            fontFamily: 'var(--font-ui)',
          }}
        >
          {empty.title}
        </div>
        {empty.hint && (
          <div
            style={{
              fontSize: 11,
              color: colors.TEXT_MUTED,
              fontFamily: 'var(--font-ui)',
              maxWidth: 360,
            }}
          >
            {empty.hint}
          </div>
        )}
      </div>
    );
  }

  return (
    <div
      ref={bodyRef}
      style={{
        flex: 1,
        minHeight: 0,
        position: 'relative',
        padding: 14,
        display: 'flex',
        flexDirection: 'column',
        gap: 8,
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
