import { useColors } from '../ThemeContext';
import { DOCS, DOC_GROUPS, GROUP_TAGLINE, type DocGroup } from '../docs-imports';
import Lightbox from './Lightbox';

interface Props {
  onSelectDoc: (id: string) => void;
}

/**
 * Docs landing — operator-journey layout.
 *
 * Five cards, ordered by the expected operator journey: Start here →
 * Deploy → Authentication → Day 2 → Reference. The old 6-feature
 * emoji grid is gone — feature marketing belongs on a product page,
 * not every docs load. The one-paragraph hero + two screenshots
 * carry orientation; everything below is task-oriented navigation.
 *
 * Sort within each group uses the `order` field from docs-imports,
 * not the title — titles change, order stays stable.
 */
export default function DocsLanding({ onSelectDoc }: Props) {
  const colors = useColors();

  const card = {
    background: colors.BG_CARD,
    border: `1px solid ${colors.BORDER}`,
    borderRadius: 10,
    padding: 20,
    transition: 'border-color 0.15s, box-shadow 0.15s',
  };

  const grouped = new Map<string, typeof DOCS>();
  for (const g of DOC_GROUPS) grouped.set(g, []);
  for (const d of DOCS) grouped.get(d.group)?.push(d);
  // Stable in-group order by `order` field.
  for (const [, docs] of grouped) docs.sort((a, b) => a.order - b.order);

  // Find the FAQ doc for the pitch-CTA link.
  const faqDoc = DOCS.find((d) => d.id === '42-faq');

  return (
    <div style={{ maxWidth: 1000, margin: '0 auto' }}>
      {/* Hero */}
      <div style={{ textAlign: 'center', padding: '40px 0 32px' }}>
        <div style={{
          fontSize: 36,
          fontWeight: 800,
          letterSpacing: -1,
          color: colors.TEXT_PRIMARY,
          fontFamily: 'var(--font-ui)',
          lineHeight: 1.2,
        }}>
          DeltaGlider Proxy
        </div>
        <div style={{
          fontSize: 16,
          color: colors.TEXT_MUTED,
          fontFamily: 'var(--font-ui)',
          marginTop: 12,
          lineHeight: 1.6,
          maxWidth: 640,
          margin: '12px auto 0',
        }}>
          An S3-compatible proxy that transparently delta-compresses versioned
          binaries, routes buckets across multiple storage backends, and
          handles authentication via SigV4 or OAuth.{' '}
          {faqDoc && (
            <a
              onClick={(e) => { e.preventDefault(); onSelectDoc(faqDoc.id); }}
              href="#"
              style={{ color: colors.ACCENT_BLUE, cursor: 'pointer' }}
            >
              What problem does this solve?
            </a>
          )}
        </div>
      </div>

      {/* Hero screenshots */}
      <div
        className="responsive-grid-2"
        style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 16, marginBottom: 40 }}
      >
        <Lightbox caption="S3 file browser with compression indicators and bulk operations">
          <img src="/_/screenshots/filebrowser.jpg" alt="Object Browser" style={{ width: '100%', display: 'block' }} />
        </Lightbox>
        <Lightbox caption="Storage analytics — per-bucket savings and cost estimation">
          <img src="/_/screenshots/analytics.jpg" alt="Analytics" style={{ width: '100%', display: 'block' }} />
        </Lightbox>
      </div>

      {/* 5-group task-oriented cards, in journey order */}
      <div
        className="responsive-grid-2"
        style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 16, marginBottom: 40 }}
      >
        {Array.from(grouped.entries()).map(([group, docs]) => (
          <div key={group} style={{ ...card, cursor: 'default' }}>
            <div style={{
              fontSize: 10,
              fontWeight: 700,
              textTransform: 'uppercase',
              letterSpacing: 1.5,
              color: colors.ACCENT_BLUE,
              fontFamily: 'var(--font-mono)',
              marginBottom: 4,
            }}>
              {group}
            </div>
            <div style={{
              fontSize: 12,
              color: colors.TEXT_MUTED,
              fontFamily: 'var(--font-ui)',
              marginBottom: 12,
              lineHeight: 1.5,
            }}>
              {GROUP_TAGLINE[group as DocGroup]}
            </div>
            {docs.map((d) => (
              <div
                key={d.id}
                onClick={() => onSelectDoc(d.id)}
                style={{
                  padding: '6px 0',
                  fontSize: 13,
                  color: colors.TEXT_SECONDARY,
                  fontFamily: 'var(--font-ui)',
                  cursor: 'pointer',
                  transition: 'color 0.1s',
                }}
                onMouseEnter={(e) => (e.currentTarget.style.color = colors.ACCENT_BLUE)}
                onMouseLeave={(e) => (e.currentTarget.style.color = colors.TEXT_SECONDARY)}
              >
                {d.title}
              </div>
            ))}
          </div>
        ))}
      </div>

      {/* Footer */}
      <div style={{
        textAlign: 'center',
        padding: '24px 0',
        fontSize: 11,
        color: colors.TEXT_FAINT,
        fontFamily: 'var(--font-mono)',
      }}>
        Built with Rust + React. Embedded in the binary.
      </div>
    </div>
  );
}
