/**
 * SectionOverview — reusable chrome for Configuration parent pages
 * (`/configuration/access`, `/storage`, `/advanced`).
 *
 * Pattern: hero strip + stat tiles + sub-section cards.
 *
 * ## Why it exists
 *
 * Before this, clicking a Configuration parent in the sidebar
 * (Access / Storage / Advanced) fell through the AdminPage
 * dispatcher and rendered the generic Dashboard — same content on
 * every page, zero orientation value. Operators couldn't tell from
 * the URL what they were looking at.
 *
 * The overview pages answer three questions the operator has when
 * they arrive:
 *
 *   1. What is this section — the hero strip's one-sentence blurb
 *      and the YAML path chip.
 *   2. What does the runtime look like RIGHT NOW — stat tiles
 *      render concrete numbers pulled from the live config
 *      (e.g. "3 buckets · 2 public", "IAM mode: GUI").
 *   3. Where do I go next — sub-section cards are sized for
 *      discoverability, each carrying its own one-sentence purpose
 *      + link, not just a label.
 *
 * The component is presentation-only. Parents own the data
 * fetching + compute the stat / card content.
 */
import type { ReactNode } from 'react';
import { RightOutlined } from '@ant-design/icons';
import { useColors } from '../ThemeContext';

export interface OverviewStat {
  /** Uppercase label for the tile. Keep under 24 chars. */
  label: string;
  /** Primary value. String (renders monospace). */
  value: string;
  /** One-line context beneath the value. */
  hint?: string;
  /** Accent colour for the value; defaults to ACCENT_BLUE. */
  accent?: string;
  /**
   * Semantic tone. When 'muted' the whole tile renders at 70%
   * opacity — used for defaults that the operator hasn't
   * customised yet.
   */
  tone?: 'normal' | 'muted' | 'warning';
}

export interface OverviewCard {
  /** Short label; matches the sidebar entry. */
  title: string;
  /** One sentence on what this sub-section owns. */
  blurb: string;
  /** Icon displayed in the card header. */
  icon: ReactNode;
  /** Route this card navigates to. */
  path: string;
  /** Compact extra info line (e.g. "3 blocks · 2 buckets"). */
  summary?: string;
  /** When true, render a yellow "YAML-managed" chip. */
  declarativeBanner?: boolean;
}

interface Props {
  /** Hero title (section name: "Access", "Storage", "Advanced"). */
  title: string;
  /**
   * One-sentence explanation of what this section covers. Shown
   * right under the title.
   */
  description: string;
  /** Icon rendered in the hero. */
  icon: ReactNode;
  /**
   * YAML path chip (e.g. `access.*`, `storage.*`, `advanced.*`).
   * Click-to-copy would be a nice-to-have; for now it's decorative.
   */
  yamlPath: string;
  /** Stat tiles rendered in a responsive 4-column grid. */
  stats: OverviewStat[];
  /** Sub-section cards rendered in a 2-column grid. */
  cards: OverviewCard[];
  /** Invoked when a card is clicked. */
  onNavigate: (path: string) => void;
}

export default function SectionOverview({
  title,
  description,
  icon,
  yamlPath,
  stats,
  cards,
  onNavigate,
}: Props) {
  const {
    BG_CARD,
    BG_ELEVATED,
    BORDER,
    TEXT_PRIMARY: TEXT,
    TEXT_SECONDARY,
    TEXT_MUTED,
    ACCENT_BLUE,
  } = useColors();

  return (
    <div
      style={{
        display: 'flex',
        flexDirection: 'column',
        gap: 24,
        maxWidth: 1080,
        margin: '0 auto',
        padding: 'clamp(16px, 3vw, 24px)',
      }}
    >
      {/* Hero */}
      <header
        style={{
          display: 'flex',
          alignItems: 'flex-start',
          gap: 20,
          padding: 'clamp(20px, 3vw, 28px)',
          background: `linear-gradient(135deg, ${BG_CARD}, ${BG_ELEVATED})`,
          border: `1px solid ${BORDER}`,
          borderRadius: 14,
        }}
      >
        <div
          style={{
            flexShrink: 0,
            width: 56,
            height: 56,
            borderRadius: 14,
            display: 'flex',
            alignItems: 'center',
            justifyContent: 'center',
            background: BG_ELEVATED,
            border: `1px solid ${BORDER}`,
            color: ACCENT_BLUE,
            fontSize: 26,
          }}
        >
          {icon}
        </div>
        <div style={{ flex: 1, minWidth: 0 }}>
          <h2
            style={{
              margin: 0,
              color: TEXT,
              fontSize: 28,
              fontFamily: 'var(--font-ui)',
              fontWeight: 700,
              letterSpacing: '-0.01em',
            }}
          >
            {title}
          </h2>
          <p
            style={{
              margin: '6px 0 10px',
              color: TEXT_SECONDARY,
              fontSize: 14,
              fontFamily: 'var(--font-ui)',
              lineHeight: 1.5,
              maxWidth: 640,
            }}
          >
            {description}
          </p>
          <span
            style={{
              display: 'inline-block',
              padding: '3px 10px',
              fontSize: 11,
              fontFamily: 'var(--font-mono)',
              background: BG_ELEVATED,
              border: `1px solid ${BORDER}`,
              borderRadius: 20,
              color: TEXT_MUTED,
            }}
            title="YAML path for this section"
          >
            {yamlPath}
          </span>
        </div>
      </header>

      {/* Stat tiles */}
      {stats.length > 0 && (
        <section
          style={{
            display: 'grid',
            gridTemplateColumns: `repeat(auto-fit, minmax(190px, 1fr))`,
            gap: 12,
          }}
          aria-label="Section overview statistics"
        >
          {stats.map((s, i) => (
            <StatTile key={i} stat={s} />
          ))}
        </section>
      )}

      {/* Sub-section cards */}
      <section
        style={{
          display: 'grid',
          gridTemplateColumns: 'repeat(auto-fit, minmax(300px, 1fr))',
          gap: 14,
        }}
        aria-label="Section sub-pages"
      >
        {cards.map((c) => (
          <SubsectionCard key={c.path} card={c} onNavigate={onNavigate} />
        ))}
      </section>
    </div>
  );
}

function StatTile({ stat }: { stat: OverviewStat }) {
  const {
    BG_CARD,
    BORDER,
    TEXT_SECONDARY,
    TEXT_MUTED,
    ACCENT_BLUE,
    ACCENT_AMBER,
  } = useColors();
  const accent =
    stat.accent ||
    (stat.tone === 'warning' ? ACCENT_AMBER : ACCENT_BLUE);
  const opacity = stat.tone === 'muted' ? 0.55 : 1;
  return (
    <div
      style={{
        background: BG_CARD,
        border: `1px solid ${BORDER}`,
        borderRadius: 12,
        padding: '14px 16px',
        opacity,
        transition: 'opacity 0.2s',
      }}
    >
      <div
        style={{
          color: TEXT_MUTED,
          fontSize: 10,
          fontWeight: 700,
          letterSpacing: 0.6,
          textTransform: 'uppercase',
          fontFamily: 'var(--font-ui)',
          marginBottom: 8,
        }}
      >
        {stat.label}
      </div>
      <div
        style={{
          color: accent,
          fontSize: 24,
          fontWeight: 700,
          fontFamily: 'var(--font-mono)',
          lineHeight: 1.2,
          // Truncate long values gracefully (listen addresses,
          // endpoint URLs) without breaking the tile grid.
          overflow: 'hidden',
          textOverflow: 'ellipsis',
          whiteSpace: 'nowrap',
        }}
        title={stat.value}
      >
        {stat.value}
      </div>
      {stat.hint && (
        <div
          style={{
            color: TEXT_SECONDARY,
            fontSize: 11,
            marginTop: 4,
            fontFamily: 'var(--font-ui)',
          }}
        >
          {stat.hint}
        </div>
      )}
    </div>
  );
}

function SubsectionCard({
  card,
  onNavigate,
}: {
  card: OverviewCard;
  onNavigate: (path: string) => void;
}) {
  const {
    BG_CARD,
    BG_ELEVATED,
    BORDER,
    TEXT_PRIMARY: TEXT,
    TEXT_SECONDARY,
    TEXT_MUTED,
    ACCENT_BLUE,
    ACCENT_AMBER,
  } = useColors();

  return (
    <button
      onClick={() => onNavigate(card.path)}
      aria-label={`${card.title} — ${card.blurb}`}
      style={{
        textAlign: 'left',
        background: BG_CARD,
        border: `1px solid ${BORDER}`,
        borderRadius: 12,
        padding: '18px 18px 16px',
        cursor: 'pointer',
        transition: 'all 0.15s ease',
        display: 'flex',
        flexDirection: 'column',
        gap: 10,
        fontFamily: 'var(--font-ui)',
        color: TEXT,
      }}
      onMouseEnter={(e) => {
        e.currentTarget.style.borderColor = ACCENT_BLUE;
        e.currentTarget.style.transform = 'translateY(-1px)';
      }}
      onMouseLeave={(e) => {
        e.currentTarget.style.borderColor = BORDER;
        e.currentTarget.style.transform = 'translateY(0)';
      }}
    >
      <div
        style={{
          display: 'flex',
          alignItems: 'center',
          gap: 12,
        }}
      >
        <div
          style={{
            flexShrink: 0,
            width: 36,
            height: 36,
            borderRadius: 10,
            display: 'flex',
            alignItems: 'center',
            justifyContent: 'center',
            background: BG_ELEVATED,
            color: ACCENT_BLUE,
            fontSize: 18,
          }}
        >
          {card.icon}
        </div>
        <div style={{ flex: 1, minWidth: 0 }}>
          <div
            style={{
              display: 'flex',
              alignItems: 'center',
              gap: 8,
              fontSize: 15,
              fontWeight: 600,
            }}
          >
            <span>{card.title}</span>
            {card.declarativeBanner && (
              <span
                style={{
                  fontSize: 9,
                  padding: '1px 6px',
                  borderRadius: 10,
                  background: `${ACCENT_AMBER}20`,
                  color: ACCENT_AMBER,
                  fontWeight: 700,
                  letterSpacing: 0.5,
                  textTransform: 'uppercase',
                }}
                title="Currently managed declaratively from YAML — mutations through this page are disabled"
              >
                YAML-managed
              </span>
            )}
          </div>
          {card.summary && (
            <div
              style={{
                fontSize: 11,
                color: TEXT_MUTED,
                fontFamily: 'var(--font-mono)',
                marginTop: 2,
              }}
            >
              {card.summary}
            </div>
          )}
        </div>
        <RightOutlined style={{ color: TEXT_MUTED, flexShrink: 0 }} />
      </div>
      <div
        style={{
          color: TEXT_SECONDARY,
          fontSize: 13,
          lineHeight: 1.5,
        }}
      >
        {card.blurb}
      </div>
    </button>
  );
}
