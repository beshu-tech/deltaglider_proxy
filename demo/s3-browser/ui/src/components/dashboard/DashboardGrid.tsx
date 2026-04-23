/**
 * DashboardGrid — the 12-column CSS Grid container for dashboard
 * panels.
 *
 * ## Why 12 columns
 *
 * Our panel roster is 8–14 per tab. 12 divides cleanly by 2, 3, 4, 6 —
 * that covers every row we actually build (halves, thirds, quarters,
 * sixths). 24 (Grafana) invites 5- and 7-col rows that don't earn
 * the added complexity at our scale.
 *
 * ## Responsiveness
 *
 * The grid has two "densities" that switch on container width, not
 * on media queries. Why: we're sometimes embedded (Admin →
 * Dashboard panel) and sometimes full-screen, so window-based
 * breakpoints don't correspond to the container the panels actually
 * live in.
 *
 *   - Comfortable: 12 logical columns, full padding, larger type.
 *     Above ~1280px container width.
 *   - Compact: 6 logical columns, tighter padding, smaller type.
 *     Between 900 and 1280px.
 *
 * Below 900px the outer admin layout has already switched to a
 * Drawer, so we let the grid collapse to 1-col via `grid-auto-flow`
 * — individual Panels read their `colSpan` as "up to 6" in that
 * mode and wrap naturally.
 *
 * Density is exposed via `data-density` on the grid and a CSS
 * custom property `--dg-density`. Panels read both; consumers only
 * need to pick their `colSpan` / `rowSpan` props.
 */
import { useEffect, useRef, useState } from 'react';
import type { ReactNode } from 'react';

type GridDensity = 'comfortable' | 'compact';

interface Props {
  children: ReactNode;
  /** Override the auto-picked density. Rare. */
  density?: GridDensity;
  /** Bottom-of-page slack so sticky actions don't cover content. */
  padBottom?: number;
}

export default function DashboardGrid({ children, density, padBottom = 24 }: Props) {
  const ref = useRef<HTMLDivElement>(null);
  const [auto, setAuto] = useState<GridDensity>('comfortable');

  useEffect(() => {
    if (density) return; // caller is driving it
    const el = ref.current;
    if (!el) return;
    // ResizeObserver: the container-width signal stays correct when
    // embedded in a panel that's narrower than the viewport.
    const ro = new ResizeObserver((entries) => {
      for (const entry of entries) {
        const w = entry.contentRect.width;
        setAuto(w >= 1280 ? 'comfortable' : 'compact');
      }
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, [density]);

  const effective: GridDensity = density ?? auto;

  return (
    <div
      ref={ref}
      data-density={effective}
      style={{
        display: 'grid',
        gridTemplateColumns: 'repeat(12, minmax(0, 1fr))',
        gridAutoRows: 'minmax(120px, auto)',
        gap: effective === 'comfortable' ? 12 : 10,
        width: '100%',
        // Outer content cap. Above 2400px we center with auto
        // margins — dashboards stop getting wider; panels get
        // denser via saved layouts (future work).
        maxWidth: 'min(100%, 2400px)',
        margin: '0 auto',
        paddingBottom: padBottom,
        // Expose density to descendants for clamp() interpolation.
        ['--dg-density' as never]: effective === 'comfortable' ? '1' : '0',
      }}
    >
      {children}
    </div>
  );
}
