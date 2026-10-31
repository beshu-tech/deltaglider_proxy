import { useEffect, useRef, type ReactNode } from 'react';
import { useColors } from '../../ThemeContext';
import './RecordList.css';
import { activateOnKey } from '../../keyboard';

/** One column of a RecordList. `track` is the wide-mode grid track size
 *  (e.g. 'max-content' | 'minmax(0,1fr)'); on narrow the list collapses to
 *  stacked cards and `label` becomes a caption above each value. */
export interface RecordColumn<T> {
  key: string;
  label: string;
  track: string;
  render: (row: T) => ReactNode;
  align?: 'start' | 'center' | 'end';
  /** Hide the per-cell caption on narrow (e.g. a self-describing meter). */
  hideLabelOnNarrow?: boolean;
  /** On narrow, take half of the card's width (short values side by side). */
  narrowHalf?: boolean;
}

/** A responsive record list: ONE DOM, ONE stylesheet. On a wide container it
 *  reads as a table (cells align across rows via CSS subgrid); the SAME markup
 *  collapses to stacked cards on a narrow container via a container query — no
 *  JS breakpoint, no conditional rendering. Adapts to its own width wherever
 *  placed (a 640px drawer, a full page). */
export default function RecordList<T>({
  rows,
  columns,
  rowKey,
  empty,
  onRowClick,
  highlightKey = null,
  isDimmed,
}: {
  rows: T[];
  columns: RecordColumn<T>[];
  rowKey: (row: T) => string;
  /** The empty state: an <EmptyState> (StatePlaceholders). */
  empty: ReactNode;
  onRowClick?: (row: T) => void;
  /** The key of a row to flash and scroll into view (a row the user just
   *  saved). */
  highlightKey?: string | null;
  /** Rows that are on their way out (a rule staged for removal): dimmed. */
  isDimmed?: (row: T) => boolean;
}) {
  const c = useColors();
  const listRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!highlightKey) return;
    const el = listRef.current?.querySelector('[data-highlight]');
    el?.scrollIntoView?.({ block: 'nearest', behavior: 'smooth' });
  }, [highlightKey, rows.length]);
  if (rows.length === 0) {
    return <div className="dg-record-empty">{empty}</div>;
  }
  const wideTemplate = columns.map((col) => col.track).join(' ');
  return (
    <div
      ref={listRef}
      className="dg-record-list"
      role="table"
      style={
        {
          '--rl-border': c.BORDER,
          '--rl-text-secondary': c.TEXT_SECONDARY,
          '--rl-text-muted': c.TEXT_MUTED,
          '--rl-bg-card': c.BG_CARD,
          '--rl-hover': c.BG_ELEVATED,
          '--rl-highlight': `${c.ACCENT_BLUE}22`,
          '--rl-cols': wideTemplate,
        } as React.CSSProperties
      }
    >
      <div className="dg-record-head" role="row">
        {columns.map((col) => (
          <div
            key={col.key}
            className="dg-record-hcell"
            role="columnheader"
            data-align={col.align}
          >
            {col.label}
          </div>
        ))}
      </div>
      {rows.map((row) => {
        const clickable = !!onRowClick;
        const key = rowKey(row);
        return (
          <div
            key={key}
            className="dg-record-row"
            role="row"
            data-highlight={highlightKey === key || undefined}
            data-dimmed={isDimmed?.(row) || undefined}
            data-clickable={clickable || undefined}
            tabIndex={clickable ? 0 : undefined}
            onClick={clickable ? () => onRowClick(row) : undefined}
            onKeyDown={clickable ? activateOnKey(() => onRowClick(row)) : undefined}
          >
            {columns.map((col) => (
              <div
                key={col.key}
                className="dg-record-cell"
                role="cell"
                data-align={col.align}
                data-col={col.key}
                data-label={col.hideLabelOnNarrow ? undefined : col.label}
                data-half={col.narrowHalf ? '' : undefined}
              >
                {col.render(row)}
              </div>
            ))}
          </div>
        );
      })}
    </div>
  );
}
