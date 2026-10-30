/**
 * BackendCard — one storage backend on the Backends page.
 *
 * Three bands, top to bottom: the header (icon, name with its status pills
 * and kind, the actions), the facts (where the data lives, how many buckets
 * route here) as a labelled list, then `children` (alerts, env notes, the
 * encryption editor). The facts never share a row with the buttons, so a
 * long endpoint or key id never wraps in a narrow column beside them.
 *
 * The layout follows the card's own width (container queries in theme.css,
 * `.dg-backend-card`), not the viewport: the admin sidebar takes a varying
 * share of the screen.
 */
import { Fragment, type ReactNode } from 'react';
import { CloudOutlined, DatabaseOutlined } from '@ant-design/icons';
import { useColors } from '../ThemeContext';
import type { BackendFact } from '../backendSummary';

export interface BackendPill {
  label: string;
  color: string;
  /** A status dot before the label (health). */
  dot?: boolean;
  title?: string;
}

interface Props {
  name: string;
  filesystem: boolean;
  kind: string;
  /** The default backend: accent border. */
  highlighted: boolean;
  pills: BackendPill[];
  facts: BackendFact[];
  /** The buttons. */
  actions: ReactNode;
  /** The "⋯" menu: the card's top-right corner at every width. */
  menu?: ReactNode;
  children?: ReactNode;
}

/** A path or URL that may wrap only after a "/" (a line break inside a
 *  folder name read as two names). `overflow-wrap: anywhere` on the cell
 *  still breaks a single name that is wider than the column. */
function breakAtSlashes(value: string): ReactNode {
  const parts = value.split('/');
  return parts.map((part, i) => (
    <Fragment key={i}>
      {part}
      {i < parts.length - 1 && (
        <>
          /<wbr />
        </>
      )}
    </Fragment>
  ));
}

function Pill({ pill }: { pill: BackendPill }) {
  return (
    <span
      title={pill.title}
      style={{
        display: 'inline-flex',
        alignItems: 'center',
        gap: 5,
        height: 20,
        padding: '0 8px',
        borderRadius: 10,
        border: `1px solid ${pill.color}55`,
        background: `${pill.color}14`,
        color: pill.color,
        fontSize: 11,
        fontWeight: 600,
        letterSpacing: 0.3,
        whiteSpace: 'nowrap',
      }}
    >
      {pill.dot && <span style={{ width: 6, height: 6, borderRadius: '50%', background: pill.color }} />}
      {pill.label}
    </span>
  );
}

export default function BackendCard({ name, filesystem, kind, highlighted, pills, facts, actions, menu, children }: Props) {
  const colors = useColors();
  return (
    <article
      data-backend-card={name}
      aria-label={`Backend ${name}`}
      className="dg-backend-card"
      style={{
        marginTop: 12,
        padding: 'clamp(12px, 2.5vw, 16px)',
        borderRadius: 10,
        border: `1px solid ${highlighted ? colors.ACCENT_BLUE + '66' : colors.BORDER}`,
        background: highlighted ? colors.ACCENT_BLUE + '08' : colors.BG_ELEVATED,
      }}
    >
      <header className="dg-backend-card__head">
        <div
          aria-hidden
          className="dg-backend-card__icon"
          style={{
            width: 32,
            height: 32,
            borderRadius: 8,
            display: 'flex',
            alignItems: 'center',
            justifyContent: 'center',
            fontSize: 16,
            color: colors.ACCENT_BLUE,
            background: `${colors.ACCENT_BLUE}14`,
            border: `1px solid ${colors.ACCENT_BLUE}22`,
          }}
        >
          {filesystem ? <DatabaseOutlined /> : <CloudOutlined />}
        </div>
        <div className="dg-backend-card__title" style={{ minWidth: 0 }}>
          <div style={{ display: 'flex', alignItems: 'center', flexWrap: 'wrap', gap: 6 }}>
            <h3
              style={{
                margin: 0,
                fontSize: 15,
                fontWeight: 600,
                fontFamily: 'var(--font-ui)',
                color: colors.TEXT_PRIMARY,
                overflowWrap: 'anywhere',
                marginRight: 2,
              }}
            >
              {name}
            </h3>
            {pills.map((p) => <Pill key={p.label} pill={p} />)}
          </div>
          <div style={{ fontSize: 12, color: colors.TEXT_MUTED, marginTop: 2 }}>{kind}</div>
        </div>
        <div className="dg-backend-card__actions">{actions}</div>
        <div className="dg-backend-card__menu">{menu}</div>
      </header>

      <div className="dg-backend-card__rule" style={{ borderTop: `1px solid ${colors.BORDER}` }} />
      <div className="dg-backend-card__body">
        <dl className="dg-backend-card__facts">
          {facts.map((f) => (
            <div key={f.label} className="dg-backend-card__fact">
              <dt style={{ color: colors.TEXT_MUTED }}>{f.label}</dt>
              <dd
                title={f.value}
                style={{
                  color: colors.TEXT_SECONDARY,
                  fontFamily: f.mono ? 'var(--font-mono)' : 'var(--font-ui)',
                  fontSize: f.mono ? 12 : 13,
                }}
              >
                {f.mono ? breakAtSlashes(f.value) : f.value}
              </dd>
            </div>
          ))}
        </dl>

        {children}
      </div>
    </article>
  );
}
