import { type ReactNode } from 'react';
import { useColors } from '../../ThemeContext';
import './RecordList.css';

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
}: {
  rows: T[];
  columns: RecordColumn<T>[];
  rowKey: (row: T) => string;
  empty: ReactNode;
  onRowClick?: (row: T) => void;
}) {
  const c = useColors();
  if (rows.length === 0) {
    return <div className="dg-record-empty">{empty}</div>;
  }
  const wideTemplate = columns.map((col) => col.track).join(' ');
  return (
    <div
      className="dg-record-list"
      role="table"
      style={
        {
          '--rl-border': c.BORDER,
          '--rl-text-secondary': c.TEXT_SECONDARY,
          '--rl-text-muted': c.TEXT_MUTED,
          '--rl-bg-card': c.BG_CARD,
          '--rl-hover': c.BG_ELEVATED,
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
        return (
          <div
            key={rowKey(row)}
            className="dg-record-row"
            role="row"
            data-clickable={clickable || undefined}
            tabIndex={clickable ? 0 : undefined}
            onClick={clickable ? () => onRowClick(row) : undefined}
            onKeyDown={
              clickable
                ? (e) => {
                    if (e.key === 'Enter' || e.key === ' ') {
                      e.preventDefault();
                      onRowClick(row);
                    }
                  }
                : undefined
            }
          >
            {columns.map((col) => (
              <div
                key={col.key}
                className="dg-record-cell"
                role="cell"
                data-align={col.align}
                data-col={col.key}
                data-label={col.hideLabelOnNarrow ? undefined : col.label}
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
