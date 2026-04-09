import { useColors } from '../ThemeContext';
import { DOCS, DOC_GROUPS } from '../docs-imports';
import Lightbox from './Lightbox';

const FEATURES = [
  { icon: '🔄', title: 'Delta Compression', desc: 'Similar files stored as xdelta3 deltas. GETs reconstruct transparently — clients never see deltas.' },
  { icon: '🪣', title: 'S3 Compatible', desc: 'Standard S3 API. Works with AWS CLI, SDKs, Cyberduck, rclone — no client changes needed.' },
  { icon: '👥', title: 'Multi-User IAM', desc: 'Per-user credentials with ABAC permissions, IP conditions, and prefix scoping.' },
  { icon: '📊', title: 'Prometheus Metrics', desc: 'Built-in metrics endpoint with request counts, latencies, cache hits, and delta ratios.' },
  { icon: '🔒', title: 'Encrypted Config', desc: 'IAM database encrypted with SQLCipher. Multi-instance sync via S3. Bootstrap password recovery.' },
  { icon: '⚡', title: 'Single Binary', desc: 'Embedded admin GUI, documentation, and S3 API — all on one port. Docker or bare metal.' },
];

interface Props {
  onSelectDoc: (id: string) => void;
}

export default function DocsLanding({ onSelectDoc }: Props) {
  const colors = useColors();

  const card = {
    background: colors.BG_CARD,
    border: `1px solid ${colors.BORDER}`,
    borderRadius: 10,
    padding: 20,
    cursor: 'pointer' as const,
    transition: 'border-color 0.15s, box-shadow 0.15s',
  };

  const grouped = new Map<string, typeof DOCS>();
  for (const g of DOC_GROUPS) grouped.set(g, []);
  for (const d of DOCS) grouped.get(d.group)?.push(d);

  return (
    <div style={{ maxWidth: 900, margin: '0 auto' }}>
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
          maxWidth: 600,
          margin: '12px auto 0',
        }}>
          S3-compatible proxy with transparent delta compression for versioned binary artifacts.
          Clients see a standard S3 API — the deduplication is invisible.
        </div>
      </div>

      {/* Screenshots */}
      <div className="responsive-grid-2" style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 16, marginBottom: 32 }}>
        <Lightbox caption="S3 object browser with compression indicators">
          <img src="/_/screenshots/filebrowser.jpg" alt="Object Browser" style={{ width: '100%', display: 'block' }} />
        </Lightbox>
        <Lightbox caption="IAM user management with ABAC permissions">
          <img src="/_/screenshots/iam.jpg" alt="IAM Users" style={{ width: '100%', display: 'block' }} />
        </Lightbox>
        <Lightbox caption="OAuth/OIDC login with Google SSO">
          <img src="/_/screenshots/oauth_login.jpg" alt="OAuth Login" style={{ width: '100%', display: 'block' }} />
        </Lightbox>
        <Lightbox caption="Storage backends with multi-backend routing">
          <img src="/_/screenshots/storage_backends.jpg" alt="Storage Backends" style={{ width: '100%', display: 'block' }} />
        </Lightbox>
        <Lightbox caption="OAuth group mapping rules">
          <img src="/_/screenshots/oauth_group_mapping.jpg" alt="Group Mapping" style={{ width: '100%', display: 'block' }} />
        </Lightbox>
        <Lightbox caption="Storage analytics and cost savings">
          <img src="/_/screenshots/analytics.jpg" alt="Analytics" style={{ width: '100%', display: 'block' }} />
        </Lightbox>
        <Lightbox caption="Advanced security settings">
          <img src="/_/screenshots/advanced_security.jpg" alt="Security Settings" style={{ width: '100%', display: 'block' }} />
        </Lightbox>
      </div>

      {/* Features grid */}
      <div className="responsive-grid-3" style={{ display: 'grid', gridTemplateColumns: 'repeat(3, 1fr)', gap: 12, marginBottom: 40 }}>
        {FEATURES.map(f => (
          <div key={f.title} style={card}>
            <div style={{ fontSize: 24, marginBottom: 8 }}>{f.icon}</div>
            <div style={{ fontSize: 14, fontWeight: 700, color: colors.TEXT_PRIMARY, fontFamily: 'var(--font-ui)', marginBottom: 6 }}>
              {f.title}
            </div>
            <div style={{ fontSize: 12, color: colors.TEXT_MUTED, fontFamily: 'var(--font-ui)', lineHeight: 1.6 }}>
              {f.desc}
            </div>
          </div>
        ))}
      </div>

      {/* Doc sections */}
      <div className="responsive-grid-2" style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 16 }}>
        {Array.from(grouped.entries()).map(([group, docs]) => (
          <div key={group} style={{ ...card, cursor: 'default' }}>
            <div style={{
              fontSize: 10,
              fontWeight: 700,
              textTransform: 'uppercase',
              letterSpacing: 1.5,
              color: colors.ACCENT_BLUE,
              fontFamily: 'var(--font-mono)',
              marginBottom: 12,
            }}>
              {group}
            </div>
            {docs.map(d => (
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
                onMouseEnter={e => e.currentTarget.style.color = colors.ACCENT_BLUE}
                onMouseLeave={e => e.currentTarget.style.color = colors.TEXT_SECONDARY}
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
        padding: '40px 0 24px',
        fontSize: 11,
        color: colors.TEXT_FAINT,
        fontFamily: 'var(--font-mono)',
      }}>
        Built with Rust + React. Embedded in the binary.
      </div>
    </div>
  );
}
